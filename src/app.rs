use crate::account::AccountStore;
use crate::agent::{Agent, claude_launch, codex_launch};
use crate::canvas::Canvas;
use crate::node::SessionNode;
use crate::session::Session;
use crate::store::{SessionRecord, Store};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use uuid::Uuid;

pub struct SessionEntry {
    pub record: SessionRecord,
    pub session: Session,
    pub node: SessionNode,
}

pub struct App {
    pub accounts: AccountStore,
    pub store_path: PathBuf,
    pub canvas: Canvas,
    pub sessions: HashMap<Uuid, SessionEntry>,
}

impl App {
    pub fn new(accounts: AccountStore, store_path: PathBuf) -> Rc<RefCell<App>> {
        Rc::new(RefCell::new(App {
            accounts,
            store_path,
            canvas: Canvas::new(),
            sessions: HashMap::new(),
        }))
    }

    /// Loads the store and spawns one Session + SessionNode per saved
    /// record, placed at its saved canvas position, wiring each node's
    /// commit signal back to its own session. Spawn failures are collected
    /// and returned so the caller can show them (e.g. as a toast) rather
    /// than losing the other sessions that did restore successfully.
    pub fn restore(app: &Rc<RefCell<App>>) -> Vec<String> {
        let (saved, load_warning) = {
            let app_ref = app.borrow();
            Store::load_with_warning(&app_ref.store_path)
        };
        let mut errors: Vec<String> = load_warning.into_iter().collect();

        {
            let app_ref = app.borrow();
            let mut state = app_ref.canvas.state.borrow_mut();
            state.zoom = saved.canvas.zoom;
            state.pan = saved.canvas.pan;
        }

        for record in saved.sessions {
            let launch = {
                let app_ref = app.borrow();
                match record.agent {
                    Agent::Claude => {
                        let account = record
                            .claude_account
                            .clone()
                            .unwrap_or_else(|| crate::account::DEFAULT_ACCOUNT.to_string());
                        match app_ref.accounts.ensure(&account) {
                            Ok(dir) => {
                                let id = record.claude_session_id.unwrap_or_else(Uuid::new_v4);
                                claude_launch(
                                    id,
                                    record.claude_session_id.is_some(),
                                    None,
                                    Some(&dir),
                                )
                            }
                            Err(error) => {
                                errors.push(format!("couldn't restore {}: {error}", record.name));
                                continue;
                            }
                        }
                    }
                    Agent::Codex => codex_launch(true, None),
                }
            };
            match Session::spawn(record.cwd.clone(), launch) {
                Ok(session) => {
                    let node = SessionNode::new(&record.name);
                    let id = record.id;
                    {
                        let app_ref = app.borrow();
                        app_ref.canvas.add_node(&node.container, record.position);
                    }
                    node.connect_commit({
                        let app = Rc::clone(app);
                        move |bytes| {
                            if let Some(entry) = app.borrow_mut().sessions.get_mut(&id) {
                                let _ = entry.session.write_input(bytes);
                            }
                        }
                    });
                    app.borrow_mut().sessions.insert(
                        id,
                        SessionEntry {
                            record,
                            session,
                            node,
                        },
                    );
                }
                Err(error) => errors.push(format!("couldn't restore {}: {error}", record.name)),
            }
        }
        errors
    }

    pub fn persist(&self) -> std::io::Result<()> {
        let state = self.canvas.state.borrow();
        let store = Store {
            sessions: self
                .sessions
                .values()
                .map(|entry| entry.record.clone())
                .collect(),
            notes: Vec::new(), // populated starting in Task 9
            links: Vec::new(), // populated starting in Task 10
            canvas: crate::store::CanvasRecord {
                zoom: state.zoom,
                pan: state.pan,
            },
        };
        store.save(&self.store_path)
    }

    /// Drains every session's PTY output into its own terminal node. Called
    /// on a timer from `main.rs`.
    pub fn pump_output(&mut self) {
        for entry in self.sessions.values_mut() {
            for chunk in entry.session.try_recv_output() {
                entry.node.feed(&chunk);
            }
        }
    }
}
