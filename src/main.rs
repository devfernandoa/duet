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
            if let Some(tab) = app.tabs.get_mut(app.focused)
                && let Err(e) = tab.write_input(&bytes)
            {
                app.last_error = Some(e.to_string());
            }
        }
    }
    false
}
