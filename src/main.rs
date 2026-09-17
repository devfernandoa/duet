mod account;
mod action;
mod agent;
mod app;
mod handoff;
mod store;
mod tab;
mod ui;

use account::AccountStore;
use action::{Action, map_key};
use agent::Agent;
use anyhow::Result;
use app::App;
use crossterm::event::{self, Event, KeyEventKind};
use std::path::PathBuf;
use std::time::Duration;

fn main() -> Result<()> {
    let store_path = store::default_store_path()?;
    let accounts_dir = store::default_accounts_dir()?;
    let accounts = AccountStore::new(accounts_dir);

    let mut terminal = ratatui::init();
    let size = terminal.size()?;
    let mut app = App::new(
        accounts,
        store_path,
        size.height.saturating_sub(2),
        size.width,
    );

    let result = run(&mut terminal, &mut app);

    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    // Draw once up front so the first frame isn't blank while waiting on the
    // first tick or event.
    terminal.draw(|frame| ui::draw(frame, app))?;
    loop {
        let mut changed = false;
        for tab in app.tabs.iter_mut() {
            if tab.pull_output() {
                changed = true;
            }
        }
        if changed {
            terminal.draw(|frame| ui::draw(frame, app))?;
        }

        if !event::poll(Duration::from_millis(33))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let quit = handle_action(app, map_key(key));
                terminal.draw(|frame| ui::draw(frame, app))?;
                if quit {
                    break;
                }
            }
            Event::Resize(cols, rows) => {
                app.resize_all(rows.saturating_sub(2), cols);
                terminal.draw(|frame| ui::draw(frame, app))?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Default name/cwd for a newly created tab.
fn new_tab_name_and_cwd(app: &App) -> std::io::Result<(String, PathBuf)> {
    let name = format!("tab-{}", app.tabs.len() + 1);
    let cwd = std::env::current_dir()?;
    Ok((name, cwd))
}

/// Returns true if the app should quit.
fn handle_action(app: &mut App, action: Action) -> bool {
    app.last_error = None;
    match action {
        Action::Quit => return true,
        // Every tab is created in the current working directory. Codex's
        // identity is `codex resume --last` filtered by cwd (not a stored
        // session id — see `codex_used` on TabRecord), so more than one
        // same-directory tab switched/created as Codex will resume and
        // summarize each other's sessions. Claude tabs don't have this
        // limitation: each gets its own pinned UUID.
        Action::NewTab => {
            let (name, cwd) = match new_tab_name_and_cwd(app) {
                Ok(v) => v,
                Err(e) => {
                    app.last_error = Some(e.to_string());
                    return false;
                }
            };
            if let Err(e) = app.new_tab(name, cwd, Agent::Claude) {
                app.last_error = Some(e.to_string());
            }
        }
        Action::NewCodexTab => {
            let (name, cwd) = match new_tab_name_and_cwd(app) {
                Ok(v) => v,
                Err(e) => {
                    app.last_error = Some(e.to_string());
                    return false;
                }
            };
            if let Err(e) = app.new_tab(name, cwd, Agent::Codex) {
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
            if let Some(tab) = app.tabs.get_mut(app.focused)
                && let Err(e) = tab.write_input(&bytes)
            {
                app.last_error = Some(e.to_string());
            }
        }
    }
    false
}
