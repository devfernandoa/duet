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
use anyhow::Result;
use app::{App, Overlay};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use std::time::Duration;

fn main() -> Result<()> {
    let store_path = store::default_store_path()?;
    let accounts_dir = store::default_accounts_dir()?;
    let accounts = AccountStore::new(accounts_dir);

    let mut terminal = ratatui::init();
    let size = terminal.size()?;
    let mut app = App::restore(
        accounts,
        store_path,
        size.height.saturating_sub(4).max(1),
        size.width.saturating_sub(32).max(2),
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
                let quit = handle_key(app, key);
                terminal.draw(|frame| ui::draw(frame, app))?;
                if quit {
                    break;
                }
            }
            Event::Resize(cols, rows) => {
                app.resize_all(
                    rows.saturating_sub(4).max(1),
                    cols.saturating_sub(32).max(2),
                );
                terminal.draw(|frame| ui::draw(frame, app))?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if app.overlay.is_some() {
        handle_overlay_key(app, key);
        return false;
    }
    handle_action(app, map_key(key))
}

fn handle_overlay_key(app: &mut App, key: KeyEvent) {
    let Some(overlay) = app.overlay.clone() else {
        return;
    };
    match overlay {
        Overlay::AccountSetup { mut name } => match key.code {
            KeyCode::Enter => {
                if let Err(error) = app.create_account(name) {
                    app.last_error = Some(error.to_string());
                }
            }
            KeyCode::Backspace => {
                name.pop();
                app.overlay = Some(Overlay::AccountSetup { name });
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                name.push(c);
                app.overlay = Some(Overlay::AccountSetup { name });
            }
            _ => {}
        },
        Overlay::NewSession {
            mut agent,
            mut account_index,
        } => match key.code {
            KeyCode::Esc => app.overlay = None,
            KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                agent = match agent {
                    agent::Agent::Claude => agent::Agent::Codex,
                    agent::Agent::Codex => agent::Agent::Claude,
                };
                app.overlay = Some(Overlay::NewSession {
                    agent,
                    account_index,
                });
            }
            KeyCode::Left => {
                let count = app.account_names().len();
                if agent == agent::Agent::Claude && count > 0 {
                    account_index = (account_index + count - 1) % count;
                }
                app.overlay = Some(Overlay::NewSession {
                    agent,
                    account_index,
                });
            }
            KeyCode::Right => {
                let count = app.account_names().len();
                if agent == agent::Agent::Claude && count > 0 {
                    account_index = (account_index + 1) % count;
                }
                app.overlay = Some(Overlay::NewSession {
                    agent,
                    account_index,
                });
            }
            KeyCode::Enter => match std::env::current_dir().and_then(|cwd| {
                app.create_selected_session(cwd)
                    .map_err(std::io::Error::other)
            }) {
                Ok(()) => {}
                Err(error) => app.last_error = Some(error.to_string()),
            },
            _ => {}
        },
    }
}

/// Returns true if the app should quit.
fn handle_action(app: &mut App, action: Action) -> bool {
    app.last_error = None;
    match action {
        Action::Quit => return true,
        Action::OpenNewSession => app.open_new_session(),
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
        Action::AddAccount => app.open_account_setup(),
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
