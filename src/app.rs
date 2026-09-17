use crate::account::{AccountStore, DEFAULT_ACCOUNT};
use crate::agent::{Agent, Launch, claude_launch, codex_launch};
use crate::handoff::{summarize_claude, summarize_codex};
use crate::store::{Store, TabRecord};
use crate::tab::Tab;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct App {
    pub tabs: Vec<Tab>,
    pub records: Vec<TabRecord>,
    pub focused: usize,
    pub last_error: Option<String>,
    accounts: AccountStore,
    store_path: PathBuf,
    rows: u16,
    cols: u16,
}

impl App {
    pub fn new(accounts: AccountStore, store_path: PathBuf, rows: u16, cols: u16) -> Self {
        App {
            tabs: Vec::new(),
            records: Vec::new(),
            focused: 0,
            last_error: None,
            accounts,
            store_path,
            rows,
            cols,
        }
    }

    /// Builds the argv and the record for a brand-new tab, without spawning
    /// anything. Pure and side-effect free (besides `self.accounts.ensure`,
    /// which just creates a directory) — this is what makes the record shape
    /// unit-testable without needing the real `claude`/`codex` binary on PATH.
    fn build_new_tab(&self, name: &str, cwd: &Path, agent: Agent) -> Result<(Launch, TabRecord)> {
        match agent {
            Agent::Claude => {
                let account = DEFAULT_ACCOUNT.to_string();
                let config_dir = self.accounts.ensure(&account)?;
                let session_id = Uuid::new_v4();
                let launch = claude_launch(session_id, false, None, Some(&config_dir));
                let record = TabRecord {
                    name: name.to_string(),
                    cwd: cwd.to_path_buf(),
                    agent,
                    claude_session_id: Some(session_id),
                    claude_account: Some(account),
                    codex_used: false,
                };
                Ok((launch, record))
            }
            Agent::Codex => {
                let launch = codex_launch(false, None);
                let record = TabRecord {
                    name: name.to_string(),
                    cwd: cwd.to_path_buf(),
                    agent,
                    claude_session_id: None,
                    claude_account: None,
                    codex_used: false,
                };
                Ok((launch, record))
            }
        }
    }

    pub fn new_tab(&mut self, name: String, cwd: PathBuf, agent: Agent) -> Result<()> {
        let (launch, record) = self.build_new_tab(&name, &cwd, agent)?;
        let tab = Tab::spawn(cwd, launch, self.rows, self.cols)?;
        self.tabs.push(tab);
        self.records.push(record);
        self.focused = self.tabs.len() - 1;
        self.persist()
    }

    pub fn close_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        self.tabs.remove(self.focused);
        self.records.remove(self.focused);
        if self.focused >= self.tabs.len() && self.focused > 0 {
            self.focused -= 1;
        }
        let _ = self.persist();
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.focused = (self.focused + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.focused = (self.focused + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    pub fn switch_agent(&mut self) -> Result<()> {
        if self.tabs.is_empty() {
            return Ok(());
        }
        let idx = self.focused;
        let record = self.records[idx].clone();

        // Drop the outgoing pty before summarizing: the CLI's non-interactive
        // summarize run needs sole ownership of the session id, which a still-live
        // interactive process for the same session would collide with.
        self.tabs[idx].kill();

        let summary = match record.agent {
            Agent::Claude => {
                let session_id = record
                    .claude_session_id
                    .context("focused tab has no Claude session to summarize")?;
                let config_dir = record
                    .claude_account
                    .as_ref()
                    .map(|a| self.accounts.config_dir(a));
                summarize_claude(session_id, config_dir.as_deref(), &record.cwd)?
            }
            Agent::Codex => summarize_codex(&record.cwd)?,
        };

        let to = match record.agent {
            Agent::Claude => Agent::Codex,
            Agent::Codex => Agent::Claude,
        };

        let (launch, mut updated) = match to {
            Agent::Claude => {
                let account = record
                    .claude_account
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
                let config_dir = self.accounts.ensure(&account)?;
                let (session_id, resume) = match record.claude_session_id {
                    Some(id) => (id, true),
                    None => (Uuid::new_v4(), false),
                };
                let launch = claude_launch(session_id, resume, Some(&summary), Some(&config_dir));
                let mut updated = record.clone();
                updated.claude_session_id = Some(session_id);
                updated.claude_account = Some(account);
                (launch, updated)
            }
            Agent::Codex => {
                let launch = codex_launch(record.codex_used, Some(&summary));
                let mut updated = record.clone();
                updated.codex_used = true;
                (launch, updated)
            }
        };
        updated.agent = to;

        let new_tab = Tab::spawn(record.cwd.clone(), launch, self.rows, self.cols)?;
        self.tabs[idx] = new_tab;
        self.records[idx] = updated;
        self.persist()
    }

    pub fn switch_account(&mut self) -> Result<()> {
        if self.tabs.is_empty() {
            return Ok(());
        }
        let idx = self.focused;
        if self.records[idx].agent != Agent::Claude {
            return Ok(());
        }
        let mut names = self.accounts.list()?;
        if names.is_empty() {
            names.push(DEFAULT_ACCOUNT.to_string());
        }
        if names.len() < 2 {
            self.last_error =
                Some("only one Claude account exists — nothing to switch to".to_string());
            return Ok(());
        }
        let current = self.records[idx]
            .claude_account
            .clone()
            .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
        let pos = names.iter().position(|n| n == &current).unwrap_or(0);
        let next_account = names[(pos + 1) % names.len()].clone();

        let config_dir = self.accounts.ensure(&next_account)?;
        let session_id = Uuid::new_v4(); // a different account is a different identity: fresh session
        let launch = claude_launch(session_id, false, None, Some(&config_dir));
        let record = self.records[idx].clone();
        let new_tab = Tab::spawn(record.cwd.clone(), launch, self.rows, self.cols)?;
        self.tabs[idx] = new_tab;
        self.records[idx].claude_account = Some(next_account);
        self.records[idx].claude_session_id = Some(session_id);
        self.persist()
    }

    pub fn restart_focused(&mut self) -> Result<()> {
        if self.tabs.is_empty() {
            return Ok(());
        }
        let idx = self.focused;
        let record = self.records[idx].clone();
        let (launch, claude_session_id) = match record.agent {
            Agent::Claude => {
                let config_dir = record
                    .claude_account
                    .as_ref()
                    .map(|a| self.accounts.config_dir(a));
                let (session_id, resume) = match record.claude_session_id {
                    Some(id) => (id, true),
                    None => (Uuid::new_v4(), false),
                };
                let launch = claude_launch(session_id, resume, None, config_dir.as_deref());
                (launch, Some(session_id))
            }
            Agent::Codex => (codex_launch(record.codex_used, None), None),
        };
        let new_tab = Tab::spawn(record.cwd.clone(), launch, self.rows, self.cols)?;
        self.tabs[idx] = new_tab;
        // A freshly-minted session id must be written back, or the next restart
        // would mint yet another one and orphan this session forever.
        if let Some(session_id) = claude_session_id {
            self.records[idx].claude_session_id = Some(session_id);
        }
        self.persist()
    }

    pub fn resize_all(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        for tab in self.tabs.iter_mut() {
            if let Err(e) = tab.resize(rows, cols) {
                self.last_error = Some(e.to_string());
            }
        }
    }

    fn persist(&self) -> Result<()> {
        let store = Store {
            tabs: self.records.clone(),
        };
        store.save(&self.store_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sh_cat_launch() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "cat".to_string()],
            envs: vec![],
        }
    }

    fn test_app() -> (App, tempfile::TempDir) {
        let tmp = tempdir().unwrap();
        let app = App::new(
            AccountStore::new(tmp.path().join("accounts")),
            tmp.path().join("tabs.json"),
            24,
            80,
        );
        (app, tmp)
    }

    fn push_fake_tab(app: &mut App, name: &str) {
        let cwd = std::env::temp_dir();
        let tab = Tab::spawn(cwd.clone(), sh_cat_launch(), 24, 80).unwrap();
        let record = TabRecord {
            name: name.to_string(),
            cwd,
            agent: Agent::Codex,
            claude_session_id: None,
            claude_account: None,
            codex_used: false,
        };
        app.tabs.push(tab);
        app.records.push(record);
        app.focused = app.tabs.len() - 1;
    }

    #[test]
    fn next_tab_wraps_around() {
        let (mut app, _tmp) = test_app();
        push_fake_tab(&mut app, "a");
        push_fake_tab(&mut app, "b");
        app.focused = 0;
        app.next_tab();
        assert_eq!(app.focused, 1);
        app.next_tab();
        assert_eq!(app.focused, 0);
    }

    #[test]
    fn prev_tab_wraps_around() {
        let (mut app, _tmp) = test_app();
        push_fake_tab(&mut app, "a");
        push_fake_tab(&mut app, "b");
        app.focused = 0;
        app.prev_tab();
        assert_eq!(app.focused, 1);
    }

    #[test]
    fn next_and_prev_tab_on_empty_app_do_not_panic() {
        let (mut app, _tmp) = test_app();
        app.next_tab();
        app.prev_tab();
        assert_eq!(app.focused, 0);
    }

    #[test]
    fn close_tab_keeps_focus_in_bounds() {
        let (mut app, _tmp) = test_app();
        push_fake_tab(&mut app, "a");
        push_fake_tab(&mut app, "b");
        app.focused = 1;
        app.close_tab();
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.focused, 0);
        assert_eq!(app.records.len(), 1);
    }

    #[test]
    fn close_tab_on_empty_app_does_not_panic() {
        let (mut app, _tmp) = test_app();
        app.close_tab();
        assert!(app.tabs.is_empty());
    }

    // build_new_tab is pure (no pty spawn), so these don't need the real
    // claude/codex binaries on PATH — unlike a full new_tab() call, which
    // does, and would fail wherever those aren't installed (e.g. CI).
    #[test]
    fn build_new_tab_for_codex_has_no_claude_identity() {
        let (app, _tmp) = test_app();
        let cwd = std::env::temp_dir();
        let (launch, record) = app.build_new_tab("t", &cwd, Agent::Codex).unwrap();
        assert_eq!(launch.program, "codex");
        assert_eq!(record.agent, Agent::Codex);
        assert!(!record.codex_used); // fresh launch, not a resume
        assert!(record.claude_session_id.is_none());
        assert!(record.claude_account.is_none());
    }

    #[test]
    fn build_new_tab_for_claude_pins_a_session_id() {
        let (app, _tmp) = test_app();
        let cwd = std::env::temp_dir();
        let (launch, record) = app.build_new_tab("t", &cwd, Agent::Claude).unwrap();
        assert_eq!(launch.program, "claude");
        assert_eq!(record.agent, Agent::Claude);
        assert!(record.claude_session_id.is_some());
        assert_eq!(record.claude_account.as_deref(), Some(DEFAULT_ACCOUNT));
    }
}
