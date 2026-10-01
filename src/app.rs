use crate::account::AccountStore;
use crate::agent::{Agent, Launch, claude_launch, codex_launch};
use crate::canvas::Canvas;
use crate::node::{NoteNode, SessionNode};
use crate::session::Session;
use crate::store::{LinkRecord, SessionRecord, StickyNoteRecord, Store};
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use uuid::Uuid;

/// How long to wait after the last edit before writing the store to disk.
/// Keeps rapid-fire events (e.g. every keystroke in a sticky note) from each
/// triggering their own synchronous `File::create` + `write_all` +
/// `sync_all` + `rename`.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

pub struct SessionEntry {
    pub record: SessionRecord,
    pub session: Session,
    pub node: SessionNode,
}

pub struct NoteEntry {
    pub record: StickyNoteRecord,
    pub node: NoteNode,
}

pub struct App {
    pub accounts: AccountStore,
    pub store_path: PathBuf,
    pub canvas: Canvas,
    pub sessions: HashMap<Uuid, SessionEntry>,
    pub notes: HashMap<Uuid, NoteEntry>,
    pub links: Vec<LinkRecord>,
    /// Set while the user has clicked a node's link button and is waiting to
    /// click a target node to complete the link. `None` otherwise.
    pub pending_link_source: Option<Uuid>,
    /// The debounce timer for `schedule_persist`, if a save is currently
    /// pending. `None` when no save is scheduled.
    pending_save: Option<glib::SourceId>,
}

impl App {
    pub fn new(accounts: AccountStore, store_path: PathBuf) -> Rc<RefCell<App>> {
        Rc::new(RefCell::new(App {
            accounts,
            store_path,
            canvas: Canvas::new(),
            sessions: HashMap::new(),
            notes: HashMap::new(),
            links: Vec::new(),
            pending_link_source: None,
            pending_save: None,
        }))
    }

    /// Debounces `persist()` so a burst of rapid-fire events (e.g. every
    /// keystroke in a sticky note's text buffer) collapses into a single
    /// disk write `PERSIST_DEBOUNCE` after the last one, instead of each
    /// event doing its own synchronous fsync+rename. Callers must still
    /// update any in-memory state (e.g. `entry.record.text`) synchronously
    /// before calling this — only the disk write is delayed.
    pub fn schedule_persist(app: &Rc<RefCell<App>>) {
        if let Some(source_id) = app.borrow_mut().pending_save.take() {
            source_id.remove();
        }
        let app_for_timeout = Rc::clone(app);
        let source_id = glib::timeout_add_local_once(PERSIST_DEBOUNCE, move || {
            let _ = app_for_timeout.borrow().persist();
            app_for_timeout.borrow_mut().pending_save = None;
        });
        app.borrow_mut().pending_save = Some(source_id);
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
                    wire_link_controls(app, &node, id);
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

        app.borrow_mut().links = saved.links;

        for note_record in saved.notes {
            let node = NoteNode::new(&note_record.text, &note_record.color);
            {
                let app_ref = app.borrow();
                app_ref.canvas.add_node(&node.container, note_record.position);
            }
            let id = note_record.id;
            node.text_view.buffer().connect_changed({
                let app = Rc::clone(app);
                move |_| {
                    if let Some(entry) = app.borrow_mut().notes.get_mut(&id) {
                        entry.record.text = entry.node.text();
                    }
                    App::schedule_persist(&app);
                }
            });
            app.borrow_mut()
                .notes
                .insert(id, NoteEntry { record: note_record, node });
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
            notes: self.notes.values().map(|entry| entry.record.clone()).collect(),
            links: self.links.clone(),
            canvas: crate::store::CanvasRecord {
                zoom: state.zoom,
                pan: state.pan,
            },
        };
        store.save(&self.store_path)
    }

    /// Drains every session's PTY output into its own terminal node, then
    /// forwards each session's chunks to any linked target sessions' input.
    /// Called on a timer from `main.rs`.
    ///
    /// Output draining and forwarding are two separate passes: the first
    /// collects `(source_id, chunks)` while holding a mutable borrow of
    /// `self.sessions` via `iter_mut`; the second looks up target sessions
    /// by id to forward into. Doing the forward inline inside the first loop
    /// would require a second mutable borrow of the same `HashMap` while the
    /// first is still live, which the borrow checker rejects.
    pub fn pump_output(&mut self) {
        let mut outgoing: Vec<(Uuid, Vec<Vec<u8>>)> = Vec::new();
        for (&id, entry) in self.sessions.iter_mut() {
            let chunks = entry.session.try_recv_output();
            for chunk in &chunks {
                entry.node.feed(chunk);
            }
            if !chunks.is_empty() {
                outgoing.push((id, chunks));
            }
        }
        for (source_id, chunks) in outgoing {
            let targets: Vec<Uuid> = self
                .links
                .iter()
                .filter(|l| l.source == source_id)
                .map(|l| l.target)
                .collect();
            for target_id in targets {
                if let Some(target_entry) = self.sessions.get_mut(&target_id) {
                    let _ = crate::link::forward(&chunks, &mut target_entry.session);
                } else {
                    // Target session is gone; drop the dangling link.
                    self.links.retain(|l| l.target != target_id);
                }
            }
        }
    }

    /// Spawns a brand-new Session + SessionNode at `viewport_center_world`,
    /// inserts it, wires its commit signal (same pattern as `restore`), and
    /// persists the store. Used by the new-session dialog in `main.rs`.
    pub fn create_session(
        app: &Rc<RefCell<App>>,
        name: String,
        cwd: PathBuf,
        agent: Agent,
        claude_account: Option<String>,
        viewport_center_world: (f64, f64),
    ) -> anyhow::Result<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("give this session a name");
        }
        if !cwd.is_dir() {
            anyhow::bail!("directory does not exist: {}", cwd.display());
        }
        {
            let app_ref = app.borrow();
            if app_ref.sessions.values().any(|entry| entry.record.name == name) {
                anyhow::bail!("a session named '{name}' already exists");
            }
        }

        let (launch, record) = {
            let app_ref = app.borrow();
            build_launch_and_record(
                &app_ref,
                &name,
                &cwd,
                agent,
                claude_account,
                viewport_center_world,
            )?
        };
        let session = Session::spawn(cwd, launch)?;
        let node = SessionNode::new(&name);
        let id = record.id;
        {
            let app_ref = app.borrow();
            app_ref.canvas.add_node(&node.container, viewport_center_world);
        }
        node.connect_commit({
            let app = Rc::clone(app);
            move |bytes| {
                if let Some(entry) = app.borrow_mut().sessions.get_mut(&id) {
                    let _ = entry.session.write_input(bytes);
                }
            }
        });
        wire_link_controls(app, &node, id);
        app.borrow_mut().sessions.insert(
            id,
            SessionEntry {
                record,
                session,
                node,
            },
        );
        app.borrow().persist()?;
        Ok(())
    }

    /// Spawns a blank yellow sticky note at `position`, inserts it, wires its
    /// text buffer's `changed` signal to update its own record and persist
    /// (same id-capture pattern as `create_session`'s commit signal), and
    /// persists the store.
    pub fn create_note(app: &Rc<RefCell<App>>, position: (f64, f64)) {
        let id = Uuid::new_v4();
        let node = NoteNode::new("", "yellow");
        {
            let app_ref = app.borrow();
            app_ref.canvas.add_node(&node.container, position);
        }
        node.text_view.buffer().connect_changed({
            let app = Rc::clone(app);
            move |_| {
                if let Some(entry) = app.borrow_mut().notes.get_mut(&id) {
                    entry.record.text = entry.node.text();
                }
                App::schedule_persist(&app);
            }
        });
        let record = StickyNoteRecord {
            id,
            text: String::new(),
            position,
            size: (220.0, 160.0),
            color: "yellow".to_string(),
        };
        app.borrow_mut().notes.insert(id, NoteEntry { record, node });
        let _ = app.borrow().persist();
    }

    /// Records a link so `pump_output` starts forwarding `source`'s output
    /// into `target`'s input. A no-op if the link already exists or
    /// `source == target` (linking a session to itself would feed its own
    /// output back into its own input).
    pub fn create_link(app: &Rc<RefCell<App>>, source: Uuid, target: Uuid) {
        let mut app = app.borrow_mut();
        if source != target && !app.links.iter().any(|l| l.source == source && l.target == target) {
            app.links.push(LinkRecord { source, target });
            let _ = app.persist();
        }
    }

    pub fn remove_link(app: &Rc<RefCell<App>>, source: Uuid, target: Uuid) {
        let mut app = app.borrow_mut();
        app.links.retain(|l| !(l.source == source && l.target == target));
        let _ = app.persist();
    }

    /// Enters "link mode" for `source`: the next node clicked (via
    /// `complete_link_if_pending`) becomes the link's target.
    pub fn start_link(app: &Rc<RefCell<App>>, source: Uuid) {
        app.borrow_mut().pending_link_source = Some(source);
    }

    /// If a link is pending (from `start_link`), completes it with `target`
    /// and clears the pending state — clearing happens unconditionally via
    /// `take()` so a later unrelated click never accidentally creates a link.
    pub fn complete_link_if_pending(app: &Rc<RefCell<App>>, target: Uuid) {
        let pending = app.borrow_mut().pending_link_source.take();
        if let Some(source) = pending {
            App::create_link(app, source, target);
        }
    }
}

/// Wires a session node's link button (click to enter link mode, sourced
/// from this node) and its terminal (click to complete a pending link,
/// targeting this node). Shared by `restore` and `create_session` so both
/// paths of session creation get link support.
///
/// The "complete pending link" gesture is attached to `node.terminal`
/// specifically, not to `node.container`. `link_button` and `terminal` are
/// siblings (both live under `container`, with `link_button` nested inside
/// `title_bar`) — if the gesture were attached to `container` instead, a
/// press on `link_button` would bubble up through `container` and fire the
/// "complete" handler *before* the button's own `clicked` signal fires on
/// release, racing against whatever was already pending and potentially
/// creating an unintended link as a side effect of merely clicking a link
/// button. Attaching to `terminal` instead means a click on `link_button`
/// (a different subtree under `container`) is never seen by this gesture at
/// all, since GTK's bubble phase only walks up a widget's own ancestor
/// chain, not into sibling subtrees.
fn wire_link_controls(app: &Rc<RefCell<App>>, node: &SessionNode, id: Uuid) {
    node.link_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::start_link(&app, id)
    });

    let click = gtk4::GestureClick::new();
    click.connect_pressed({
        let app = Rc::clone(app);
        move |_gesture, _n_press, _x, _y| {
            App::complete_link_if_pending(&app, id);
        }
    });
    node.terminal.add_controller(click);
}

fn build_launch_and_record(
    app: &App,
    name: &str,
    cwd: &Path,
    agent: Agent,
    claude_account: Option<String>,
    position: (f64, f64),
) -> anyhow::Result<(Launch, SessionRecord)> {
    match agent {
        Agent::Claude => {
            let account = claude_account.unwrap_or_else(|| crate::account::DEFAULT_ACCOUNT.to_string());
            let config_dir = app.accounts.ensure(&account)?;
            let session_id = Uuid::new_v4();
            let launch = claude_launch(session_id, false, None, Some(&config_dir));
            let record = SessionRecord {
                id: Uuid::new_v4(),
                name: name.to_string(),
                cwd: cwd.to_path_buf(),
                agent,
                claude_session_id: Some(session_id),
                claude_account: Some(account),
                position,
                size: (480.0, 320.0),
            };
            Ok((launch, record))
        }
        Agent::Codex => {
            let launch = codex_launch(false, None);
            let record = SessionRecord {
                id: Uuid::new_v4(),
                name: name.to_string(),
                cwd: cwd.to_path_buf(),
                agent,
                claude_session_id: None,
                claude_account: None,
                position,
                size: (480.0, 320.0),
            };
            Ok((launch, record))
        }
    }
}
