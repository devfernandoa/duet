# duet Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `duet`, a Rust TUI that runs `claude` and `codex` in named, persistent tabs, with a summarize-and-handoff flow for switching the agent inside a tab and per-tab Claude account isolation.

**Architecture:** One `ratatui` binary. Every open tab owns a real pty (`portable-pty`) running one agent, rendered through a `vt100` screen buffer (`tui-term`'s `PseudoTerminal` widget). Tabs and Claude accounts persist to plain files under the XDG data dir. Switching an agent inside a tab runs the outgoing agent non-interactively to get a text summary, then launches the incoming agent with that summary as its opening prompt.

**Tech Stack:** Rust 2024 edition, `ratatui` 0.30, `crossterm` 0.29, `tui-term` 0.3, `portable-pty` 0.9, `vt100` (via `tui-term`, default feature), `serde`/`serde_json`, `uuid` (v4 + serde), `dirs`, `anyhow`. Dev-only: `tempfile`.

**Spec:** [`docs/superpowers/specs/2026-09-17-duet-design.md`](../specs/2026-09-17-duet-design.md)

## Global Constraints

- Rust 2024 edition, matches the already-committed `Cargo.toml`. Do not add dependencies beyond the tech stack above without updating the spec first.
- No integration test spawns the real `claude` or `codex` binaries — CI (`.github/workflows/ci.yml`) runs on a bare `ubuntu-latest` runner with neither installed. Tests that need a real child process use `sh`, which is present on every POSIX runner.
- Every task keeps the crate compiling and `cargo test` green. Because early tasks add `pub` functions nothing calls yet, `main.rs` carries a temporary `#![allow(dead_code)]` crate-level attribute (added in Task 1) that Task 9 removes once everything is wired up — that is the only place clippy is allowed to be non-strict mid-plan. Every task still runs `cargo test`; only the full `cargo fmt --check && cargo clippy --all-targets -- -D warnings` (the CI gate) is deferred to Task 9.
- Run `cargo fmt` before every commit.
- Codex session resume never needs a stored session id: `codex resume --last` and `codex exec resume --last` filter candidates by the process's current working directory (verified via `codex resume --help` / `codex exec resume --help` — `--all` is documented as the flag that "disables cwd filtering"). So a tab's Codex identity is just "has Codex been launched in this tab before" (`bool`), and the pty/summarize commands must always run with `cwd` set to the tab's directory.
- Claude session resume DOES need a stored id: `duet` generates a `Uuid` per tab the first time that tab launches Claude, passes `--session-id <uuid>` that first time, `--resume <uuid>` every time after.

---

### Task 1: `agent.rs` — Agent enum and argv building

**Files:**
- Modify: `src/main.rs` (add `#![allow(dead_code)]` at the very top, then `mod agent;`)
- Create: `src/agent.rs`

**Interfaces:**
- Produces: `pub enum Agent { Claude, Codex }` (derives `Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize`); `pub struct Launch { pub program: String, pub args: Vec<String>, pub envs: Vec<(String, String)> }` (derives `Debug, Clone, PartialEq, Eq`); `impl Launch { pub fn to_std_command(&self) -> std::process::Command }`; `pub const SUMMARY_PROMPT: &str`; `pub fn claude_launch(session_id: uuid::Uuid, resume: bool, initial_prompt: Option<&str>, config_dir: Option<&std::path::Path>) -> Launch`; `pub fn codex_launch(resume: bool, initial_prompt: Option<&str>) -> Launch`; `pub fn claude_summarize_launch(session_id: uuid::Uuid, config_dir: Option<&std::path::Path>) -> Launch`; `pub fn codex_summarize_launch() -> Launch`.

- [ ] **Step 1: Write the failing tests**

Create `src/agent.rs` with just the module doc and this test module (no implementation yet):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use uuid::Uuid;

    #[test]
    fn claude_fresh_launch_pins_session_id() {
        let id = Uuid::nil();
        let launch = claude_launch(id, false, None, None);
        assert_eq!(launch.program, "claude");
        assert_eq!(launch.args, vec!["--session-id", &id.to_string()]);
        assert!(launch.envs.is_empty());
    }

    #[test]
    fn claude_resume_launch_uses_resume_flag() {
        let id = Uuid::nil();
        let launch = claude_launch(id, true, None, None);
        assert_eq!(launch.args, vec!["--resume", &id.to_string()]);
    }

    #[test]
    fn claude_launch_appends_initial_prompt() {
        let id = Uuid::nil();
        let launch = claude_launch(id, true, Some("hello"), None);
        assert_eq!(launch.args, vec!["--resume", &id.to_string(), "hello"]);
    }

    #[test]
    fn claude_launch_injects_config_dir_env() {
        let id = Uuid::nil();
        let dir = Path::new("/tmp/duet-accounts/work");
        let launch = claude_launch(id, false, None, Some(dir));
        assert_eq!(
            launch.envs,
            vec![("CLAUDE_CONFIG_DIR".to_string(), "/tmp/duet-accounts/work".to_string())]
        );
    }

    #[test]
    fn codex_fresh_launch_has_no_resume_flags() {
        let launch = codex_launch(false, Some("hi"));
        assert_eq!(launch.program, "codex");
        assert_eq!(launch.args, vec!["hi"]);
    }

    #[test]
    fn codex_resume_launch_uses_resume_last() {
        let launch = codex_launch(true, Some("hi"));
        assert_eq!(launch.args, vec!["resume", "--last", "hi"]);
    }

    #[test]
    fn codex_resume_launch_without_prompt() {
        let launch = codex_launch(true, None);
        assert_eq!(launch.args, vec!["resume", "--last"]);
    }

    #[test]
    fn claude_summarize_uses_print_mode_and_resume() {
        let id = Uuid::nil();
        let launch = claude_summarize_launch(id, None);
        assert_eq!(launch.program, "claude");
        assert_eq!(launch.args[0], "-p");
        assert_eq!(launch.args[1], "--resume");
        assert_eq!(launch.args[2], id.to_string());
        assert_eq!(launch.args[3], SUMMARY_PROMPT);
    }

    #[test]
    fn codex_summarize_uses_exec_resume_last() {
        let launch = codex_summarize_launch();
        assert_eq!(launch.program, "codex");
        assert_eq!(launch.args, vec!["exec", "resume", "--last", SUMMARY_PROMPT]);
    }

    #[test]
    fn to_std_command_sets_program_args_and_envs() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "true".to_string()],
            envs: vec![("FOO".to_string(), "bar".to_string())],
        };
        let cmd = launch.to_std_command();
        assert_eq!(cmd.get_program(), "sh");
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("true")]
        );
        assert_eq!(
            cmd.get_envs().collect::<Vec<_>>(),
            vec![(std::ffi::OsStr::new("FOO"), Some(std::ffi::OsStr::new("bar")))]
        );
    }
}
```

Add `mod agent;` to `src/main.rs`, keeping the default `fn main() { println!("Hello, world!"); }` body. Add `#![allow(dead_code)]` as the very first line of `src/main.rs`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo build`
Expected: FAIL to compile — `claude_launch`, `codex_launch`, `claude_summarize_launch`, `codex_summarize_launch`, `Launch`, `SUMMARY_PROMPT` are not defined.

- [ ] **Step 3: Implement `agent.rs`**

Add above the `#[cfg(test)]` module:

```rust
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Agent {
    Claude,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
}

impl Launch {
    pub fn to_std_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args);
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
        cmd
    }
}

pub const SUMMARY_PROMPT: &str =
    "Summarize this session in a few sentences so another agent can continue the work.";

fn claude_config_env(config_dir: Option<&Path>) -> Vec<(String, String)> {
    match config_dir {
        Some(dir) => vec![("CLAUDE_CONFIG_DIR".to_string(), dir.to_string_lossy().to_string())],
        None => Vec::new(),
    }
}

pub fn claude_launch(
    session_id: Uuid,
    resume: bool,
    initial_prompt: Option<&str>,
    config_dir: Option<&Path>,
) -> Launch {
    let mut args = vec![
        if resume { "--resume" } else { "--session-id" }.to_string(),
        session_id.to_string(),
    ];
    if let Some(prompt) = initial_prompt {
        args.push(prompt.to_string());
    }
    Launch {
        program: "claude".to_string(),
        args,
        envs: claude_config_env(config_dir),
    }
}

pub fn codex_launch(resume: bool, initial_prompt: Option<&str>) -> Launch {
    let mut args = Vec::new();
    if resume {
        args.push("resume".to_string());
        args.push("--last".to_string());
    }
    if let Some(prompt) = initial_prompt {
        args.push(prompt.to_string());
    }
    Launch {
        program: "codex".to_string(),
        args,
        envs: Vec::new(),
    }
}

pub fn claude_summarize_launch(session_id: Uuid, config_dir: Option<&Path>) -> Launch {
    Launch {
        program: "claude".to_string(),
        args: vec![
            "-p".to_string(),
            "--resume".to_string(),
            session_id.to_string(),
            SUMMARY_PROMPT.to_string(),
        ],
        envs: claude_config_env(config_dir),
    }
}

pub fn codex_summarize_launch() -> Launch {
    Launch {
        program: "codex".to_string(),
        args: vec![
            "exec".to_string(),
            "resume".to_string(),
            "--last".to_string(),
            SUMMARY_PROMPT.to_string(),
        ],
        envs: Vec::new(),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test agent::`
Expected: PASS, all 10 tests green.

- [ ] **Step 5: Commit**

```bash
cd /home/fernando/Documents/duet
cargo fmt
git add src/agent.rs src/main.rs
git commit -m "Add agent.rs: argv building for claude/codex launch and summarize"
```

---

### Task 2: `account.rs` — filesystem-backed Claude account registry

**Files:**
- Modify: `src/main.rs` (add `mod account;`)
- Create: `src/account.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `pub const DEFAULT_ACCOUNT: &str = "default"`; `pub struct AccountStore { .. }` with `pub fn new(root: std::path::PathBuf) -> Self`, `pub fn config_dir(&self, name: &str) -> std::path::PathBuf`, `pub fn ensure(&self, name: &str) -> std::io::Result<std::path::PathBuf>`, `pub fn list(&self) -> std::io::Result<Vec<String>>`.

- [ ] **Step 1: Add the `tempfile` dev-dependency**

```bash
cd /home/fernando/Documents/duet
cargo add tempfile --dev
```

- [ ] **Step 2: Write the failing tests**

Create `src/account.rs`:

```rust
use std::io;
use std::path::PathBuf;

pub const DEFAULT_ACCOUNT: &str = "default";

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn ensure_creates_directory_and_list_finds_it_sorted() {
        let tmp = tempdir().unwrap();
        let store = AccountStore::new(tmp.path().join("accounts"));
        store.ensure("work").unwrap();
        store.ensure("personal").unwrap();
        assert_eq!(store.list().unwrap(), vec!["personal".to_string(), "work".to_string()]);
    }

    #[test]
    fn list_on_missing_root_is_empty() {
        let tmp = tempdir().unwrap();
        let store = AccountStore::new(tmp.path().join("nonexistent"));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn config_dir_joins_root_and_name() {
        let store = AccountStore::new(PathBuf::from("/tmp/duet-test-root"));
        assert_eq!(store.config_dir("work"), PathBuf::from("/tmp/duet-test-root/work"));
    }

    #[test]
    fn ensure_is_idempotent() {
        let tmp = tempdir().unwrap();
        let store = AccountStore::new(tmp.path().join("accounts"));
        store.ensure("work").unwrap();
        store.ensure("work").unwrap();
        assert_eq!(store.list().unwrap(), vec!["work".to_string()]);
    }
}
```

Add `mod account;` to `src/main.rs`.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test account::`
Expected: FAIL to compile — `AccountStore` is not defined.

- [ ] **Step 4: Implement `AccountStore`**

Add above the test module:

```rust
pub struct AccountStore {
    root: PathBuf,
}

impl AccountStore {
    pub fn new(root: PathBuf) -> Self {
        AccountStore { root }
    }

    pub fn config_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn ensure(&self, name: &str) -> io::Result<PathBuf> {
        let dir = self.config_dir(name);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn list(&self) -> io::Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = std::fs::read_dir(&self.root)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(names)
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test account::`
Expected: PASS, all 4 tests green.

- [ ] **Step 6: Commit**

```bash
cd /home/fernando/Documents/duet
cargo fmt
git add src/account.rs src/main.rs Cargo.toml Cargo.lock
git commit -m "Add account.rs: filesystem-backed Claude account registry"
```

---

### Task 3: `store.rs` — tab persistence

**Files:**
- Modify: `src/main.rs` (add `mod store;`)
- Create: `src/store.rs`

**Interfaces:**
- Consumes: `crate::agent::Agent` (Task 1).
- Produces: `pub struct TabRecord { pub name: String, pub cwd: PathBuf, pub agent: Agent, pub claude_session_id: Option<Uuid>, pub claude_account: Option<String>, pub codex_used: bool }` (derives `Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize`); `pub struct Store { pub tabs: Vec<TabRecord> }` (derives `Debug, Default, serde::Serialize, serde::Deserialize`) with `pub fn load(path: &Path) -> Store`, `pub fn save(&self, path: &Path) -> std::io::Result<()>`; `pub fn default_store_path() -> anyhow::Result<PathBuf>`; `pub fn default_accounts_dir() -> anyhow::Result<PathBuf>`.

- [ ] **Step 1: Write the failing tests**

Create `src/store.rs`:

```rust
use crate::agent::Agent;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_record() -> TabRecord {
        TabRecord {
            name: "web".to_string(),
            cwd: PathBuf::from("/home/fernando/web"),
            agent: Agent::Claude,
            claude_session_id: Some(Uuid::nil()),
            claude_account: Some("work".to_string()),
            codex_used: false,
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("tabs.json");
        let store = Store { tabs: vec![sample_record()] };
        store.save(&path).unwrap();
        let loaded = Store::load(&path);
        assert_eq!(loaded.tabs, vec![sample_record()]);
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("tabs.json");
        let store = Store { tabs: vec![sample_record()] };
        store.save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        assert!(Store::load(&path).tabs.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("tabs.json");
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(Store::load(&path).tabs.is_empty());
    }
}
```

Add `mod store;` to `src/main.rs`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test store::`
Expected: FAIL to compile — `TabRecord` and `Store` are not defined.

- [ ] **Step 3: Implement `TabRecord`, `Store`, and the path helpers**

Add above the test module:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabRecord {
    pub name: String,
    pub cwd: PathBuf,
    pub agent: Agent,
    pub claude_session_id: Option<Uuid>,
    pub claude_account: Option<String>,
    pub codex_used: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub tabs: Vec<TabRecord>,
}

impl Store {
    pub fn load(path: &Path) -> Store {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Store::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).expect("Store always serializes");
        std::fs::write(path, json)
    }
}

pub fn default_store_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("no data directory available on this platform"))?
        .join("duet");
    Ok(dir.join("tabs.json"))
}

pub fn default_accounts_dir() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("no data directory available on this platform"))?
        .join("duet")
        .join("accounts");
    Ok(dir)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test store::`
Expected: PASS, all 4 tests green.

- [ ] **Step 5: Commit**

```bash
cd /home/fernando/Documents/duet
cargo fmt
git add src/store.rs src/main.rs
git commit -m "Add store.rs: JSON persistence of tab records"
```

---

### Task 4: `tab.rs` — pty-backed Tab

**Files:**
- Modify: `src/main.rs` (add `mod tab;`)
- Create: `src/tab.rs`

**Interfaces:**
- Consumes: `crate::agent::{Agent, Launch}` (Task 1).
- Produces: `pub struct Tab { .. }` with `pub fn spawn(name: String, cwd: PathBuf, agent: Agent, launch: Launch, rows: u16, cols: u16) -> anyhow::Result<Tab>`, `pub fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<()>`, `pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()>`, `pub fn pull_output(&mut self) -> bool` (also caches child exit status as a side effect — see below), `pub fn screen(&self) -> &vt100::Screen`, `pub fn exit_status(&self) -> Option<&portable_pty::ExitStatus>`, plus public fields `pub name: String`, `pub cwd: PathBuf`, `pub agent: Agent`. Also `impl Drop for Tab` (kills the child).

`exit_status()` is how Task 8's `ui.rs` implements the spec's "child process exits unexpectedly → pane shows exit status and a restart hint" requirement — `pull_output` is called every frame in the Task 9 run loop, so it is the natural place to notice a dead child without a separate poll.

- [ ] **Step 1: Write the failing tests**

Create `src/tab.rs`:

```rust
use crate::agent::{Agent, Launch};
use portable_pty::{native_pty_system, CommandBuilder, ChildKiller, PtyPair, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_hello() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "printf hello".to_string()],
            envs: vec![],
        }
    }

    fn sleeper() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 5".to_string()],
            envs: vec![],
        }
    }

    #[test]
    fn spawn_reads_child_output_into_screen() {
        let mut tab = Tab::spawn(
            "t".to_string(),
            std::env::temp_dir(),
            Agent::Codex,
            echo_hello(),
            24,
            80,
        )
        .unwrap();

        let mut seen = String::new();
        for _ in 0..50 {
            tab.pull_output();
            seen = tab.screen().contents();
            if seen.contains("hello") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(seen.contains("hello"), "expected pty output to contain 'hello', got: {seen:?}");
    }

    #[test]
    fn resize_updates_screen_dimensions() {
        let mut tab = Tab::spawn(
            "t".to_string(),
            std::env::temp_dir(),
            Agent::Codex,
            sleeper(),
            24,
            80,
        )
        .unwrap();
        tab.resize(30, 100).unwrap();
        assert_eq!(tab.screen().size(), (30, 100));
    }

    #[test]
    fn pull_output_detects_child_exit() {
        let mut tab = Tab::spawn(
            "t".to_string(),
            std::env::temp_dir(),
            Agent::Codex,
            echo_hello(),
            24,
            80,
        )
        .unwrap();
        let mut status = None;
        for _ in 0..50 {
            tab.pull_output();
            status = tab.exit_status().cloned();
            if status.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(status.is_some(), "expected child to have exited by now");
    }

    #[test]
    fn exit_status_is_none_while_child_is_running() {
        let mut tab = Tab::spawn(
            "t".to_string(),
            std::env::temp_dir(),
            Agent::Codex,
            sleeper(),
            24,
            80,
        )
        .unwrap();
        tab.pull_output();
        assert!(tab.exit_status().is_none());
    }

    #[test]
    fn spawn_with_missing_binary_errors() {
        let launch = Launch {
            program: "duet-does-not-exist-binary".to_string(),
            args: vec![],
            envs: vec![],
        };
        let result = Tab::spawn("t".to_string(), std::env::temp_dir(), Agent::Codex, launch, 24, 80);
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test tab::`
Expected: FAIL to compile — `Tab` is not defined.

- [ ] **Step 3: Implement `Tab`**

Add above the test module:

```rust
pub struct Tab {
    pub name: String,
    pub cwd: PathBuf,
    pub agent: Agent,
    pair: PtyPair,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    parser: vt100::Parser,
    exit_status: Option<portable_pty::ExitStatus>,
}

impl Tab {
    pub fn spawn(
        name: String,
        cwd: PathBuf,
        agent: Agent,
        launch: Launch,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Tab> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&launch.program);
        cmd.args(&launch.args);
        for (key, value) in &launch.envs {
            cmd.env(key, value);
        }
        cmd.cwd(&cwd);

        let child = pair.slave.spawn_command(cmd)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx) = channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let parser = vt100::Parser::new(rows, cols, 10_000);

        Ok(Tab {
            name,
            cwd,
            agent,
            pair,
            child,
            writer,
            output_rx: rx,
            parser,
            exit_status: None,
        })
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        self.pair.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser.screen_mut().set_size(rows, cols);
        Ok(())
    }

    pub fn pull_output(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.output_rx.try_recv() {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    changed = true;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if self.exit_status.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exit_status = Some(status);
                changed = true;
            }
        }
        changed
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn exit_status(&self) -> Option<&portable_pty::ExitStatus> {
        self.exit_status.as_ref()
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
```

Add `mod tab;` to `src/main.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test tab::`
Expected: PASS, all 6 tests green. (The output/exit tests poll for up to 1 second; they should pass well before that on any reasonable machine.)

- [ ] **Step 5: Commit**

```bash
cd /home/fernando/Documents/duet
cargo fmt
git add src/tab.rs src/main.rs
git commit -m "Add tab.rs: pty-backed Tab wrapping portable-pty and vt100"
```

---

### Task 5: `handoff.rs` — summarize-and-capture

**Files:**
- Modify: `src/main.rs` (add `mod handoff;`)
- Create: `src/handoff.rs`

**Interfaces:**
- Consumes: `crate::agent::{Launch, claude_summarize_launch, codex_summarize_launch}` (Task 1).
- Produces: `pub fn run_and_capture(launch: &Launch, cwd: &Path) -> anyhow::Result<String>`; `pub fn summarize_claude(session_id: uuid::Uuid, config_dir: Option<&Path>, cwd: &Path) -> anyhow::Result<String>`; `pub fn summarize_codex(cwd: &Path) -> anyhow::Result<String>`.

- [ ] **Step 1: Write the failing tests**

Create `src/handoff.rs`:

```rust
use crate::agent::Launch;
use anyhow::{bail, Context, Result};
use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_and_capture_returns_trimmed_stdout() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "printf 'hello world\\n'".to_string()],
            envs: vec![],
        };
        let result = run_and_capture(&launch, &std::env::temp_dir()).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn run_and_capture_errors_on_nonzero_exit() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 3".to_string()],
            envs: vec![],
        };
        assert!(run_and_capture(&launch, &std::env::temp_dir()).is_err());
    }

    #[test]
    fn run_and_capture_errors_on_empty_output() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "true".to_string()],
            envs: vec![],
        };
        assert!(run_and_capture(&launch, &std::env::temp_dir()).is_err());
    }

    #[test]
    fn run_and_capture_errors_on_missing_binary() {
        let launch = Launch {
            program: "duet-does-not-exist-binary".to_string(),
            args: vec![],
            envs: vec![],
        };
        assert!(run_and_capture(&launch, &std::env::temp_dir()).is_err());
    }

    #[test]
    fn run_and_capture_sets_working_directory() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "pwd".to_string()],
            envs: vec![],
        };
        let tmp = std::env::temp_dir();
        let result = run_and_capture(&launch, &tmp).unwrap();
        // Canonicalize both sides: on macOS /tmp is a symlink, and this keeps the
        // assertion honest on any platform without hardcoding a path shape.
        assert_eq!(
            std::fs::canonicalize(result).unwrap(),
            std::fs::canonicalize(&tmp).unwrap()
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test handoff::`
Expected: FAIL to compile — `run_and_capture` is not defined.

- [ ] **Step 3: Implement `run_and_capture` and the two summarize wrappers**

Add above the test module:

```rust
pub fn run_and_capture(launch: &Launch, cwd: &Path) -> Result<String> {
    let mut cmd = launch.to_std_command();
    cmd.current_dir(cwd);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run {}", launch.program))?;
    if !output.status.success() {
        bail!(
            "{} exited with {}: {}",
            launch.program,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        bail!("{} produced no output", launch.program);
    }
    Ok(text)
}

pub fn summarize_claude(session_id: uuid::Uuid, config_dir: Option<&Path>, cwd: &Path) -> Result<String> {
    run_and_capture(&crate::agent::claude_summarize_launch(session_id, config_dir), cwd)
}

pub fn summarize_codex(cwd: &Path) -> Result<String> {
    run_and_capture(&crate::agent::codex_summarize_launch(), cwd)
}
```

Add `mod handoff;` to `src/main.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test handoff::`
Expected: PASS, all 5 tests green.

- [ ] **Step 5: Commit**

```bash
cd /home/fernando/Documents/duet
cargo fmt
git add src/handoff.rs src/main.rs
git commit -m "Add handoff.rs: non-interactive summarize-and-capture"
```

---

### Task 6: `action.rs` — keybinding-to-action mapping

**Files:**
- Modify: `src/main.rs` (add `mod action;`)
- Create: `src/action.rs`

**Interfaces:**
- Consumes: `crossterm::event::KeyEvent` (already a project dependency).
- Produces: `pub enum Action { NewTab, CloseTab, NextTab, PrevTab, SwitchAgent, SwitchAccount, RestartTab, Quit, Forward(Vec<u8>) }` (derives `Debug, Clone, PartialEq, Eq`); `pub fn map_key(key: crossterm::event::KeyEvent) -> Action`.

- [ ] **Step 1: Write the failing tests**

Create `src/action.rs`:

```rust
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_t_maps_to_new_tab() {
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::NewTab);
    }

    #[test]
    fn ctrl_w_maps_to_close_tab() {
        let key = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::CloseTab);
    }

    #[test]
    fn ctrl_right_and_left_map_to_next_and_prev_tab() {
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
            Action::NextTab
        );
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            Action::PrevTab
        );
    }

    #[test]
    fn ctrl_a_maps_to_switch_agent() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::SwitchAgent);
    }

    #[test]
    fn ctrl_g_maps_to_switch_account() {
        let key = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::SwitchAccount);
    }

    #[test]
    fn ctrl_r_maps_to_restart_tab() {
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::RestartTab);
    }

    #[test]
    fn ctrl_q_maps_to_quit() {
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::Quit);
    }

    #[test]
    fn plain_char_forwards_utf8_bytes() {
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(map_key(key), Action::Forward(vec![b'x']));
    }

    #[test]
    fn enter_forwards_carriage_return() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(map_key(key), Action::Forward(vec![b'\r']));
    }

    #[test]
    fn backspace_forwards_del_byte() {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(map_key(key), Action::Forward(vec![0x7f]));
    }

    #[test]
    fn arrow_keys_forward_escape_sequences() {
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Action::Forward(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn unmodified_char_that_collides_with_a_ctrl_binding_is_not_special() {
        // Plain 't' (no Ctrl) must just be forwarded, not treated as NewTab.
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(map_key(key), Action::Forward(vec![b't']));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test action::`
Expected: FAIL to compile — `Action` and `map_key` are not defined.

- [ ] **Step 3: Implement `Action` and `map_key`**

Add above the test module:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SwitchAgent,
    SwitchAccount,
    RestartTab,
    Quit,
    Forward(Vec<u8>),
}

pub fn map_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('t') => return Action::NewTab,
            KeyCode::Char('w') => return Action::CloseTab,
            KeyCode::Right => return Action::NextTab,
            KeyCode::Left => return Action::PrevTab,
            KeyCode::Char('a') => return Action::SwitchAgent,
            KeyCode::Char('g') => return Action::SwitchAccount,
            KeyCode::Char('r') => return Action::RestartTab,
            KeyCode::Char('q') => return Action::Quit,
            _ => {}
        }
    }
    Action::Forward(key_to_bytes(key))
}

fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let byte = (c.to_ascii_uppercase() as u8).wrapping_sub(b'@');
                vec![byte]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        _ => Vec::new(),
    }
}
```

Add `mod action;` to `src/main.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test action::`
Expected: PASS, all 11 tests green.

- [ ] **Step 5: Commit**

```bash
cd /home/fernando/Documents/duet
cargo fmt
git add src/action.rs src/main.rs
git commit -m "Add action.rs: pure keybinding-to-Action mapping"
```

---

### Task 7: `app.rs` — tab lifecycle and orchestration

**Files:**
- Modify: `src/main.rs` (add `mod app;`)
- Create: `src/app.rs`

**Interfaces:**
- Consumes: `crate::account::{AccountStore, DEFAULT_ACCOUNT}` (Task 2), `crate::agent::{Agent, claude_launch, codex_launch}` (Task 1), `crate::handoff::{summarize_claude, summarize_codex}` (Task 5), `crate::store::{Store, TabRecord, default_store_path, default_accounts_dir}` (Task 3), `crate::tab::Tab` (Task 4).
- Produces: `pub struct App { pub tabs: Vec<Tab>, pub records: Vec<TabRecord>, pub focused: usize, pub last_error: Option<String>, .. }` with `pub fn new(accounts: AccountStore, store_path: PathBuf, rows: u16, cols: u16) -> Self`, `pub fn new_tab(&mut self, name: String, cwd: PathBuf, agent: Agent) -> anyhow::Result<()>`, `pub fn close_tab(&mut self)`, `pub fn next_tab(&mut self)`, `pub fn prev_tab(&mut self)`, `pub fn switch_agent(&mut self) -> anyhow::Result<()>`, `pub fn switch_account(&mut self) -> anyhow::Result<()>`, `pub fn restart_focused(&mut self) -> anyhow::Result<()>`.

- [ ] **Step 1: Write the failing tests**

Create `src/app.rs`:

```rust
use crate::account::{AccountStore, DEFAULT_ACCOUNT};
use crate::agent::{claude_launch, codex_launch, Agent};
use crate::handoff::{summarize_claude, summarize_codex};
use crate::store::{Store, TabRecord};
use crate::tab::Tab;
use anyhow::{Context, Result};
use std::path::PathBuf;
use uuid::Uuid;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Launch;
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
        let tab = Tab::spawn(name.to_string(), cwd.clone(), Agent::Codex, sh_cat_launch(), 24, 80).unwrap();
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

    #[test]
    fn new_tab_with_codex_agent_persists_a_record() {
        let (mut app, _tmp) = test_app();
        app.new_tab("t".to_string(), std::env::temp_dir(), Agent::Codex).unwrap();
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.records[0].agent, Agent::Codex);
        assert!(!app.records[0].codex_used); // fresh launch, not a resume
        assert!(app.store_path.exists());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test app::`
Expected: FAIL to compile — `App` is not defined (and `store_path` is referenced in the last test before the field exists).

- [ ] **Step 3: Implement `App`**

Add above the test module:

```rust
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

    pub fn new_tab(&mut self, name: String, cwd: PathBuf, agent: Agent) -> Result<()> {
        let (launch, record) = match agent {
            Agent::Claude => {
                let account = DEFAULT_ACCOUNT.to_string();
                let config_dir = self.accounts.ensure(&account)?;
                let session_id = Uuid::new_v4();
                let launch = claude_launch(session_id, false, None, Some(&config_dir));
                let record = TabRecord {
                    name: name.clone(),
                    cwd: cwd.clone(),
                    agent,
                    claude_session_id: Some(session_id),
                    claude_account: Some(account),
                    codex_used: false,
                };
                (launch, record)
            }
            Agent::Codex => {
                let launch = codex_launch(false, None);
                let record = TabRecord {
                    name: name.clone(),
                    cwd: cwd.clone(),
                    agent,
                    claude_session_id: None,
                    claude_account: None,
                    codex_used: false,
                };
                (launch, record)
            }
        };
        let tab = Tab::spawn(name, cwd, agent, launch, self.rows, self.cols)?;
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

        let summary = match record.agent {
            Agent::Claude => {
                let session_id = record
                    .claude_session_id
                    .context("focused tab has no Claude session to summarize")?;
                let config_dir = record.claude_account.as_ref().map(|a| self.accounts.config_dir(a));
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
                let account = record.claude_account.clone().unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
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

        let new_tab = Tab::spawn(record.name.clone(), record.cwd.clone(), to, launch, self.rows, self.cols)?;
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
        let current = self.records[idx].claude_account.clone().unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
        let pos = names.iter().position(|n| n == &current).unwrap_or(0);
        let next_account = names[(pos + 1) % names.len()].clone();

        let config_dir = self.accounts.ensure(&next_account)?;
        let session_id = Uuid::new_v4(); // a different account is a different identity: fresh session
        let launch = claude_launch(session_id, false, None, Some(&config_dir));
        let record = self.records[idx].clone();
        let new_tab = Tab::spawn(record.name.clone(), record.cwd.clone(), Agent::Claude, launch, self.rows, self.cols)?;
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
        let launch = match record.agent {
            Agent::Claude => {
                let config_dir = record.claude_account.as_ref().map(|a| self.accounts.config_dir(a));
                let (session_id, resume) = match record.claude_session_id {
                    Some(id) => (id, true),
                    None => (Uuid::new_v4(), false),
                };
                claude_launch(session_id, resume, None, config_dir.as_deref())
            }
            Agent::Codex => codex_launch(record.codex_used, None),
        };
        let new_tab = Tab::spawn(record.name.clone(), record.cwd.clone(), record.agent, launch, self.rows, self.cols)?;
        self.tabs[idx] = new_tab;
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        let store = Store { tabs: self.records.clone() };
        store.save(&self.store_path)?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test app::`
Expected: PASS, all 6 tests green.

- [ ] **Step 5: Commit**

```bash
cd /home/fernando/Documents/duet
cargo fmt
git add src/app.rs src/main.rs
git commit -m "Add app.rs: tab lifecycle, agent switching, account switching"
```

---

### Task 8: `ui.rs` — rendering

**Files:**
- Modify: `src/main.rs` (add `mod ui;`)
- Create: `src/ui.rs`

**Interfaces:**
- Consumes: `crate::app::App` (Task 7), `crate::agent::Agent` (Task 1), `tui_term::widget::PseudoTerminal`, `ratatui::Frame`.
- Produces: `pub fn draw(frame: &mut ratatui::Frame, app: &App)`.

This module is pure UI composition with no branching worth a unit test beyond what `cargo build` already checks (a bad layout call is a compile error or a visibly broken screen, not a silent logic bug) — verified manually in Task 9's run-through instead of with an automated test, per the spec's testing section (no framework/fixtures beyond what's asked).

- [ ] **Step 1: Implement `ui.rs`**

Create `src/ui.rs`:

```rust
use crate::agent::Agent;
use crate::app::App;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use tui_term::widget::PseudoTerminal;

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    draw_tab_bar(frame, app, chunks[0]);
    draw_focused_pane(frame, app, chunks[1]);
    draw_status_line(frame, app, chunks[2]);
}

fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    for (i, record) in app.records.iter().enumerate() {
        let label = format!(" {}:{} ", record.name, agent_label(record.agent));
        let style = if i == app.focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(label, style));
    }
    if spans.is_empty() {
        spans.push(Span::raw(" no tabs — Ctrl+T to create one "));
    }
    frame.render_widget(Line::from(spans), area);
}

fn draw_focused_pane(frame: &mut Frame, app: &App, area: Rect) {
    match app.tabs.get(app.focused) {
        Some(tab) => {
            if let Some(status) = tab.exit_status() {
                let msg = format!("process exited ({status})\n\nPress Ctrl+R to restart this tab.");
                frame.render_widget(Paragraph::new(msg), area);
            } else {
                let pseudo_term = PseudoTerminal::new(tab.screen());
                frame.render_widget(pseudo_term, area);
            }
        }
        None => {
            frame.render_widget(
                Paragraph::new("No tabs open. Press Ctrl+T to create one, Ctrl+Q to quit."),
                area,
            );
        }
    }
}

fn draw_status_line(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(err) = &app.last_error {
        frame.render_widget(
            Paragraph::new(format!("error: {err}")).style(Style::default().fg(Color::Red)),
            area,
        );
        return;
    }

    let text = match app.records.get(app.focused) {
        Some(record) => format!(
            "{} [{}{}]  Ctrl+T new  Ctrl+W close  Ctrl+\u{2190}/\u{2192} switch tab  Ctrl+A switch agent  Ctrl+G switch account  Ctrl+R restart  Ctrl+Q quit",
            record.name,
            agent_label(record.agent),
            record
                .claude_account
                .as_ref()
                .map(|a| format!(":{a}"))
                .unwrap_or_default(),
        ),
        None => "Ctrl+T new tab  Ctrl+Q quit".to_string(),
    };
    frame.render_widget(Paragraph::new(text).style(Style::default().fg(Color::DarkGray)), area);
}

fn agent_label(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
    }
}
```

Add `mod ui;` to `src/main.rs`. Note this needs `app.tabs`, `app.records`, `app.focused`, `app.last_error` to be readable from another module — they already are `pub` fields on `App` from Task 7.

- [ ] **Step 2: Verify it compiles**

Run: `cargo build`
Expected: builds clean (there is no test to run for this task).

- [ ] **Step 3: Commit**

```bash
cd /home/fernando/Documents/duet
cargo fmt
git add src/ui.rs src/main.rs
git commit -m "Add ui.rs: tab bar, focused pane, status line rendering"
```

---

### Task 9: `main.rs` — terminal bootstrap and event loop

**Files:**
- Modify: `src/main.rs` (replace the whole file)

**Interfaces:**
- Consumes: every module from Tasks 1–8.
- Produces: the `duet` binary's `main()`.

- [ ] **Step 1: Replace `src/main.rs`**

At this point `src/main.rs` is just the accumulated `mod` declarations plus `#![allow(dead_code)]` and the default `fn main()`. Replace its entire contents with:

```rust
mod account;
mod action;
mod agent;
mod app;
mod handoff;
mod store;
mod tab;
mod ui;

use account::AccountStore;
use action::{map_key, Action};
use agent::Agent;
use anyhow::Result;
use app::App;
use crossterm::event::{self, Event, KeyEventKind};
use std::time::Duration;

fn main() -> Result<()> {
    let store_path = store::default_store_path()?;
    let accounts_dir = store::default_accounts_dir()?;
    let accounts = AccountStore::new(accounts_dir);

    let mut terminal = ratatui::init();
    let size = terminal.size()?;
    let mut app = App::new(accounts, store_path, size.height.saturating_sub(2), size.width);

    let result = run(&mut terminal, &mut app);

    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        for tab in app.tabs.iter_mut() {
            tab.pull_output();
        }
        terminal.draw(|frame| ui::draw(frame, app))?;

        if !event::poll(Duration::from_millis(33))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if handle_action(app, map_key(key)) {
                    break;
                }
            }
            Event::Resize(cols, rows) => {
                let pty_rows = rows.saturating_sub(2);
                for tab in app.tabs.iter_mut() {
                    if let Err(e) = tab.resize(pty_rows, cols) {
                        app.last_error = Some(e.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Returns true if the app should quit.
fn handle_action(app: &mut App, action: Action) -> bool {
    app.last_error = None;
    match action {
        Action::Quit => return true,
        Action::NewTab => {
            let name = format!("tab-{}", app.tabs.len() + 1);
            let cwd = match std::env::current_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    app.last_error = Some(e.to_string());
                    return false;
                }
            };
            if let Err(e) = app.new_tab(name, cwd, Agent::Claude) {
                app.last_error = Some(e.to_string());
            }
        }
        Action::CloseTab => app.close_tab(),
        Action::NextTab => app.next_tab(),
        Action::PrevTab => app.prev_tab(),
        Action::SwitchAgent => {
            if let Err(e) = app.switch_agent() {
                app.last_error = Some(e.to_string());
            }
        }
        Action::SwitchAccount => {
            if let Err(e) = app.switch_account() {
                app.last_error = Some(e.to_string());
            }
        }
        Action::RestartTab => {
            if let Err(e) = app.restart_focused() {
                app.last_error = Some(e.to_string());
            }
        }
        Action::Forward(bytes) => {
            if let Some(tab) = app.tabs.get_mut(app.focused) {
                if let Err(e) = tab.write_input(&bytes) {
                    app.last_error = Some(e.to_string());
                }
            }
        }
    }
    false
}
```

This removes `#![allow(dead_code)]` (it was only ever in the file this step replaces) — every `pub` item added in Tasks 1–8 is now reachable from `main`.

- [ ] **Step 2: Run the full test suite**

Run: `cargo test`
Expected: PASS, every test from every module (agent, account, store, tab, handoff, action, app) green, no leftover references to removed code.

- [ ] **Step 3: Run the full CI gate locally**

```bash
cd /home/fernando/Documents/duet
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build
cargo test
```

Expected: all four commands succeed with zero warnings. If clippy flags anything (e.g. a genuinely unused helper), fix it now — this is the first point where dead code has nowhere to hide, which is the point of deferring the strict gate to this task.

- [ ] **Step 4: Manual smoke test**

Run: `cargo run`

Verify by hand (this exercises real `claude`/`codex`, which is why it's manual and not automated):
- `Ctrl+T` opens a new Claude tab in the current directory; typing reaches the agent.
- `Ctrl+T` again opens a second tab; `Ctrl+Right`/`Ctrl+Left` cycles focus between them.
- `Ctrl+A` on a tab summarizes the current agent and reopens the tab on the other agent with that summary as the opening message.
- `Ctrl+G` on a Claude tab cycles between accounts under `~/.local/share/duet/accounts/` (a brand-new account name will show Claude's own login flow — that's expected, not a bug).
- `Ctrl+W` closes the focused tab; `Ctrl+Q` quits and restores the terminal cleanly (no leftover raw-mode terminal).
- Type `exit` (or the agent's own quit command) inside a tab so its process exits on its own: the pane should switch to the "process exited (...)" message instead of silently freezing, and `Ctrl+R` should bring it back to life.
- Quit and run `cargo run` again: tabs persisted in `~/.local/share/duet/tabs.json` should be visible in that file (re-opening them into live tabs on startup is intentionally out of scope for this plan — see Follow-ups).

- [ ] **Step 5: Commit and push**

```bash
cd /home/fernando/Documents/duet
cargo fmt
git add src/main.rs
git commit -m "Wire up main.rs: terminal bootstrap, event loop, action dispatch"
git push
```

Confirm CI is green on GitHub after the push (`gh run watch` or check the Actions tab) before considering the plan complete.

---

## Follow-ups (explicitly out of scope for this plan)

- **Restoring persisted tabs on startup.** `Store::load` and `TabRecord` already carry everything needed (`claude_session_id`/`claude_account` for `--resume`, `codex_used` for `resume --last`), but wiring "offer to reopen tabs from `tabs.json` at launch" into `main.rs` is a separate, independently-testable chunk of work — do it as a follow-up plan once the base app is in daily use and its UX (auto-reopen vs. a picker) can be judged from real usage.
- **Codex per-account isolation.** Spec and this plan scope multi-account handling to Claude only, per the original request.
