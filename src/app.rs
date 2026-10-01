use crate::account::AccountStore;
use crate::agent::{Agent, Launch, claude_launch, codex_launch};
use crate::canvas::Canvas;
use crate::handoff::{summarize_claude, summarize_codex};
use crate::node::{NoteNode, SessionNode};
use crate::session::Session;
use crate::store::{LinkRecord, SessionRecord, StickyNoteRecord, Store};
use anyhow::Context;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
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
    /// The character grid last pushed to this session's PTY, so
    /// `pump_output`'s sync only sends a `SIGWINCH` when the grid actually
    /// changed. `None` means "never synced" (the PTY still has
    /// `Session::spawn`'s 80x24).
    pub pty_grid: Option<(u16, u16)>,
    /// Whether the "exited" status badge has already been shown for this
    /// session. Set once by `pump_output` the first time `exit_status()`
    /// becomes `Some`, so the label is set once rather than every tick.
    pub exit_shown: bool,
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
    /// The link the user clicked on the canvas, highlighted and armed so a
    /// second click on it deletes it. `None` when nothing is selected.
    pub selected_link: Option<LinkRecord>,
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
            selected_link: None,
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
    /// `toast_overlay` is threaded down into `wire_link_controls` so a later
    /// failed handoff on a restored session can surface its own toast.
    pub fn restore(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) -> Vec<String> {
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
                    // Ask for the saved size; `pump_output` syncs the PTY to
                    // whatever grid VTE actually ends up rendering.
                    node.request_grid(record.size.0, record.size.1);
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
                    wire_link_controls(app, &node, id, toast_overlay);
                    wire_session_chrome(app, &node, id);
                    wire_rename(app, &node, id, toast_overlay);
                    app.borrow_mut().sessions.insert(
                        id,
                        SessionEntry {
                            record,
                            session,
                            node,
                            pty_grid: None,
                            exit_shown: false,
                        },
                    );
                }
                Err(error) => errors.push(format!("couldn't restore {}: {error}", record.name)),
            }
        }

        app.borrow_mut().links = saved.links;

        for note_record in saved.notes {
            let node = NoteNode::new(&note_record.text, &note_record.color);
            node.text_view
                .set_size_request(note_record.size.0 as i32, note_record.size.1 as i32);
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
            wire_note_chrome(app, &node, id);
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
            // The one authoritative place the PTY is sized, from VTE's real
            // post-allocation grid. Every other path (resize drag, restore,
            // create, collapse/expand, a font change) only ever asks the
            // terminal for a size request — see `SessionNode::request_grid`
            // for why a request is not what VTE ends up rendering, and why
            // resizing the PTY to the *requested* grid garbles the text of a
            // card narrower than its own title bar.
            if let Some(grid) = entry.node.actual_grid() {
                if entry.pty_grid != Some(grid) {
                    let _ = entry.session.resize(grid.1, grid.0);
                    entry.pty_grid = Some(grid);
                }
            }
            let chunks = entry.session.try_recv_output();
            for chunk in &chunks {
                entry.node.feed(chunk);
            }
            if !entry.exit_shown && entry.session.exit_status().is_some() {
                entry.node.status_label.set_text("exited");
                entry.exit_shown = true;
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
        toast_overlay: &adw::ToastOverlay,
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
        node.request_grid(record.size.0, record.size.1);
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
        wire_link_controls(app, &node, id, toast_overlay);
        wire_session_chrome(app, &node, id);
        wire_rename(app, &node, id, toast_overlay);
        app.borrow_mut().sessions.insert(
            id,
            SessionEntry {
                record,
                session,
                node,
                pty_grid: None,
                exit_shown: false,
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
        wire_note_chrome(app, &node, id);
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
    /// into `target`'s input. A no-op if the link already exists,
    /// `source == target` (linking a session to itself would feed its own
    /// output back into its own input), or either id no longer names a live
    /// session — this last check is defense in depth against a stale
    /// `pending_link_source` surviving the source session's closure (the
    /// primary fix for that is clearing `pending_link_source` in
    /// `close_session` itself; this is a second, independent guard so
    /// `create_link` can never persist a link to a session id that doesn't
    /// exist, regardless of what let it get called that way).
    /// Returns whether a new link was actually recorded, so the caller can
    /// tell the user something happened (see `complete_link_if_pending`).
    pub fn create_link(app: &Rc<RefCell<App>>, source: Uuid, target: Uuid) -> bool {
        let mut app = app.borrow_mut();
        if source != target
            && app.sessions.contains_key(&source)
            && app.sessions.contains_key(&target)
            && !app.links.iter().any(|l| l.source == source && l.target == target)
        {
            app.links.push(LinkRecord { source, target });
            let _ = app.persist();
            return true;
        }
        false
    }

    /// Puts the `link-source` CSS class on exactly the card named by
    /// `pending_link_source`, and on no other. Link mode previously had *no*
    /// observable effect whatsoever — `start_link` only wrote a field — so
    /// clicking the link button genuinely looked like it did nothing.
    ///
    /// Written as a full sweep rather than an add-here/remove-there pair so a
    /// highlight can never be left behind on a card whose pending link was
    /// cleared by some other path (`close_session`, a completed link).
    fn refresh_link_highlight(&self) {
        for (&id, entry) in &self.sessions {
            if self.pending_link_source == Some(id) {
                entry.node.container.add_css_class("link-source");
            } else {
                entry.node.container.remove_css_class("link-source");
            }
        }
    }

    /// Every live link's world-space endpoints: out of the source card's
    /// right edge, into the target card's left edge, each at the card's
    /// vertical middle. One function so the curve that gets drawn and the
    /// curve a click is hit-tested against can never disagree.
    pub fn link_lines(&self) -> Vec<(LinkRecord, (f64, f64), (f64, f64))> {
        self.links
            .iter()
            .filter_map(|&link| {
                let source = self.sessions.get(&link.source)?;
                let target = self.sessions.get(&link.target)?;
                Some((
                    link,
                    card_edge(&source.record, true),
                    card_edge(&target.record, false),
                ))
            })
            .collect()
    }

    /// Whether any card covers `world` (world-space). Used to keep a click
    /// that landed on a node from also being treated as a click on a link
    /// line, since the lines are painted behind the nodes.
    fn covers_point(&self, world: (f64, f64)) -> bool {
        let inside = |position: (f64, f64), size: (f64, f64)| {
            world.0 >= position.0
                && world.0 <= position.0 + size.0
                && world.1 >= position.1
                && world.1 <= position.1 + size.1 + TITLE_BAR_HEIGHT
        };
        self.sessions
            .values()
            .any(|entry| inside(entry.record.position, entry.record.size))
            || self
                .notes
                .values()
                .any(|entry| inside(entry.record.position, entry.record.size))
    }

    /// Click-to-select, click-again-to-delete for link lines — the pattern
    /// the design spec asks for ("click a link line to select it, then a
    /// key/button to delete"). A second click rather than a keypress because
    /// a card's terminal takes the keyboard (more so now that hovering grabs
    /// focus), so a `Delete` binding would be swallowed by whatever agent is
    /// running. A click that misses every link just clears the selection.
    ///
    /// Returns a toast message when something happened; `None` when the
    /// click was a plain deselect, which needs no announcement.
    pub fn click_link_at(app: &Rc<RefCell<App>>, world: (f64, f64)) -> Option<String> {
        let (hit, previous) = {
            let app_ref = app.borrow();
            // A fixed 12px of grab slack on screen, converted to world units
            // so a link is no harder to hit when zoomed out.
            let tolerance = 12.0 / app_ref.canvas.state.borrow().zoom;
            // Links are drawn *behind* the cards, so a click that landed on a
            // card is never a click on a link — without this, clicking near
            // the right edge of a terminal (where a link's source anchor is)
            // would select the link leaving it.
            let hit = if app_ref.covers_point(world) {
                None
            } else {
                app_ref
                    .link_lines()
                    .into_iter()
                    .map(|(link, from, to)| {
                        (link, crate::canvas::distance_to_link(from, to, world))
                    })
                    .filter(|(_, distance)| *distance <= tolerance)
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(link, _)| link)
            };
            (hit, app_ref.selected_link)
        };
        match hit {
            None => {
                app.borrow_mut().selected_link = None;
                None
            }
            Some(link) if previous == Some(link) => {
                App::remove_link(app, link.source, link.target);
                app.borrow_mut().selected_link = None;
                let app_ref = app.borrow();
                let name = |id: Uuid| {
                    app_ref
                        .sessions
                        .get(&id)
                        .map(|entry| entry.record.name.clone())
                        .unwrap_or_default()
                };
                Some(format!(
                    "unlinked {} -> {}",
                    name(link.source),
                    name(link.target)
                ))
            }
            Some(link) => {
                app.borrow_mut().selected_link = Some(link);
                Some("link selected — click it again to delete".to_string())
            }
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
        app.borrow().refresh_link_highlight();
    }

    /// If a link is pending (from `start_link`), completes it with `target`
    /// and clears the pending state — clearing happens unconditionally via
    /// `take()` so a later unrelated click never accidentally creates a link.
    ///
    /// Returns a description of the link that was created, for the caller to
    /// surface as a toast. Completing a link otherwise has no visible effect
    /// at all (nothing in the UI draws `links`), so without this the second
    /// half of linking looked just as dead as the first.
    pub fn complete_link_if_pending(app: &Rc<RefCell<App>>, target: Uuid) -> Option<String> {
        let source = app.borrow_mut().pending_link_source.take()?;
        let created = App::create_link(app, source, target);
        let app_ref = app.borrow();
        app_ref.refresh_link_highlight();
        if !created {
            return None;
        }
        let name = |id: Uuid| {
            app_ref
                .sessions
                .get(&id)
                .map(|entry| entry.record.name.clone())
                .unwrap_or_default()
        };
        Some(format!("linked {} -> {}", name(source), name(target)))
    }

    /// Commits an inline title rename (see `wire_rename`). Applies the same
    /// uniqueness rule as `App::create_session`, since the name is what the
    /// user identifies a session by and two cards called the same thing would
    /// be indistinguishable on the canvas. Nothing else keys off the name —
    /// records are keyed by `Uuid` — so a rename is just the record, the
    /// label, and a save.
    pub fn rename_session(app: &Rc<RefCell<App>>, id: Uuid, name: &str) -> anyhow::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("give this session a name");
        }
        {
            let app_ref = app.borrow();
            if app_ref
                .sessions
                .iter()
                .any(|(&other, entry)| other != id && entry.record.name == name)
            {
                anyhow::bail!("a session named '{name}' already exists");
            }
        }
        {
            let mut app_mut = app.borrow_mut();
            let entry = app_mut.sessions.get_mut(&id).context("session not found")?;
            entry.record.name = name.to_string();
            entry.node.set_name(name);
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// Hands a session off from its current agent to the other one: kills
    /// the running process, asks it (via `handoff.rs`) to summarize the
    /// conversation, then relaunches the other agent in the *same* node at
    /// the same canvas position with that summary as its initial prompt. On
    /// a failed summarize, falls back to a generic "start fresh" prompt
    /// rather than failing the whole handoff.
    pub fn switch_agent(app: &Rc<RefCell<App>>, id: Uuid) -> anyhow::Result<()> {
        let record = {
            let mut app_mut = app.borrow_mut();
            let entry = app_mut.sessions.get_mut(&id).context("session not found")?;
            entry.session.kill();
            entry.record.clone()
        };

        let summary_result = match record.agent {
            Agent::Claude => {
                let session_id = record
                    .claude_session_id
                    .context("session has no Claude session to summarize")?;
                let config_dir = record
                    .claude_account
                    .as_ref()
                    .map(|a| app.borrow().accounts.config_dir(a));
                summarize_claude(session_id, config_dir.as_deref(), &record.cwd)
            }
            Agent::Codex => summarize_codex(&record.cwd),
        };
        let summary = match summary_result {
            Ok(summary) => summary,
            Err(_) => "The previous agent session could not be recovered. Start by inspecting the working directory and continue from there.".to_string(),
        };

        let to = match record.agent {
            Agent::Claude => Agent::Codex,
            Agent::Codex => Agent::Claude,
        };
        let (launch, updated_record) = {
            let app_ref = app.borrow();
            match to {
                Agent::Claude => {
                    let account = crate::account::DEFAULT_ACCOUNT.to_string();
                    let config_dir = app_ref.accounts.ensure(&account)?;
                    let session_id = Uuid::new_v4();
                    let launch = claude_launch(session_id, false, Some(&summary), Some(&config_dir));
                    let mut updated = record.clone();
                    updated.agent = Agent::Claude;
                    updated.claude_session_id = Some(session_id);
                    updated.claude_account = Some(account);
                    (launch, updated)
                }
                Agent::Codex => {
                    let launch = codex_launch(false, Some(&summary));
                    let mut updated = record.clone();
                    updated.agent = Agent::Codex;
                    updated.claude_session_id = None;
                    updated.claude_account = None;
                    (launch, updated)
                }
            }
        };
        let new_session = Session::spawn(updated_record.cwd.clone(), launch)?;
        {
            let mut app_mut = app.borrow_mut();
            if let Some(entry) = app_mut.sessions.get_mut(&id) {
                entry.session = new_session;
                entry.record = updated_record;
            }
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// Removes every session using account `name`, then the account's
    /// on-disk config directory via `AccountStore::remove`. Uses
    /// `canvas.remove_node` (not `canvas.fixed.remove` directly) so the node
    /// is also dropped from `Canvas`'s internal position-tracking list —
    /// otherwise it would leak there and every future pan/zoom would keep
    /// repositioning a widget that's no longer in the `Fixed` container.
    pub fn delete_account(app: &Rc<RefCell<App>>, name: &str) -> anyhow::Result<()> {
        let to_remove: Vec<Uuid> = {
            let app_ref = app.borrow();
            app_ref
                .sessions
                .iter()
                .filter(|(_, entry)| entry.record.claude_account.as_deref() == Some(name))
                .map(|(id, _)| *id)
                .collect()
        };
        {
            let mut app_mut = app.borrow_mut();
            for id in &to_remove {
                if let Some(mut entry) = app_mut.sessions.remove(id) {
                    entry.session.kill();
                    app_mut.canvas.remove_node(&entry.node.container);
                }
            }
        }
        app.borrow().accounts.remove(name)?;
        app.borrow().persist()?;
        Ok(())
    }

    /// Removes a single session from the canvas (its per-card close button):
    /// kills the process, drops the node via `canvas.remove_node`, drops any
    /// link to/from it so `pump_output` never looks it up again, and clears
    /// `pending_link_source` if it was this session — otherwise clicking
    /// this session's link button and then closing it (instead of completing
    /// the link) would leave `pending_link_source == Some(id)` dangling, and
    /// the next unrelated click-to-complete on any other session would call
    /// `create_link` with a source id that no longer exists.
    pub fn close_session(app: &Rc<RefCell<App>>, id: Uuid) {
        {
            let mut app_mut = app.borrow_mut();
            if let Some(mut entry) = app_mut.sessions.remove(&id) {
                entry.session.kill();
                app_mut.canvas.remove_node(&entry.node.container);
            }
            app_mut.links.retain(|l| l.source != id && l.target != id);
            if app_mut.pending_link_source == Some(id) {
                app_mut.pending_link_source = None;
            }
            if app_mut
                .selected_link
                .is_some_and(|l| l.source == id || l.target == id)
            {
                app_mut.selected_link = None;
            }
            app_mut.refresh_link_highlight();
        }
        let _ = app.borrow().persist();
    }

    /// Removes a single sticky note from the canvas (its per-note close
    /// button).
    pub fn close_note(app: &Rc<RefCell<App>>, id: Uuid) {
        {
            let mut app_mut = app.borrow_mut();
            if let Some(entry) = app_mut.notes.remove(&id) {
                app_mut.canvas.remove_node(&entry.node.container);
            }
        }
        let _ = app.borrow().persist();
    }
}

/// Roughly the height a card's title bar adds above its body. Only used to
/// put a link line's endpoint near the vertical middle of a card rather than
/// its top edge; a few pixels either way is invisible on a link.
const TITLE_BAR_HEIGHT: f64 = 28.0;

/// A card's left or right edge at its vertical middle, in world space.
/// `record.position` is the card's top-left and `record.size` is its *body*
/// size, hence the title-bar correction.
fn card_edge(record: &SessionRecord, right: bool) -> (f64, f64) {
    (
        if right {
            record.position.0 + record.size.0
        } else {
            record.position.0
        },
        record.position.1 + (record.size.1 + TITLE_BAR_HEIGHT) / 2.0,
    )
}

/// Click-to-rename: the title label swaps for an entry pre-filled with the
/// current name, Enter commits (through `App::rename_session`, which enforces
/// the same uniqueness rule as creating a session), Escape reverts without
/// saving. Replaces the F4 rename dialog the old ratatui UI had, which was
/// never carried over to the GTK rewrite.
///
/// The click gesture goes on the label itself rather than the title bar, for
/// the same reason `wire_link_controls` attaches to `node.terminal`: a
/// gesture on the shared ancestor would also fire for presses on the
/// title bar's buttons and its drag handle.
fn wire_rename(
    app: &Rc<RefCell<App>>,
    node: &SessionNode,
    id: Uuid,
    toast_overlay: &adw::ToastOverlay,
) {
    let click = gtk4::GestureClick::new();
    click.connect_released({
        let label = node.title_label.clone();
        let entry = node.title_entry.clone();
        move |_gesture, _n_press, _x, _y| crate::node::set_renaming(&label, &entry, true)
    });
    node.title_label.add_controller(click);

    let finish = {
        let label = node.title_label.clone();
        let entry = node.title_entry.clone();
        move || crate::node::set_renaming(&label, &entry, false)
    };

    node.title_entry.connect_activate({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        let finish = finish.clone();
        move |entry| {
            if let Err(error) = App::rename_session(&app, id, &entry.text()) {
                toast_overlay.add_toast(adw::Toast::new(&error.to_string()));
                // Deliberately stays in edit mode on a rejected name (empty,
                // or a duplicate) so the typed text isn't thrown away.
                return;
            }
            finish();
        }
    });

    let keys = gtk4::EventControllerKey::new();
    // Capture phase so Escape is seen before `GtkEntry`'s own internal text
    // widget gets a chance at it; everything else is passed straight through
    // so normal typing is untouched.
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let finish = finish.clone();
        move |_controller, key, _code, _modifiers| {
            if key == gtk4::gdk::Key::Escape {
                finish();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    node.title_entry.add_controller(keys);
}

/// A node body's real current size in pre-zoom pixels (GTK allocations are in
/// the widget's own untransformed space, so this is directly comparable to the
/// `size` stored in a record), or `None` if it isn't allocated yet.
///
/// Resize drags use this rather than `record.size` as their starting point,
/// which is the fix for the "card shrank and can't be dragged back up" state.
/// The two quantities drift apart, because a card's rendered width is
/// `max(record.size.0, title-bar minimum)`: a 28-character session name
/// measured 348px of title bar, so a card whose record said 220 actually drew
/// 348 wide. Starting a resize from the stale 220 meant the first 128px of
/// rightward drag changed nothing visible at all, and every fresh drag
/// restarted from the same stale number — the card read as un-growable. The
/// title label is now ellipsized (see `SessionNode::new`) so the title bar
/// stops forcing a minimum, but anchoring the drag to the real size is what
/// makes the gesture self-correcting regardless of where a mismatch comes
/// from (grid rounding, a future wider title bar, a restored record).
fn allocated_size(widget: &impl IsA<gtk4::Widget>) -> Option<(f64, f64)> {
    let widget = widget.as_ref();
    let (width, height) = (widget.width(), widget.height());
    (width > 0 && height > 0).then_some((width as f64, height as f64))
}

/// Expresses `local` — a point in the coordinate space of the widget
/// `gesture` is attached to — in the canvas `Fixed`'s coordinate space.
fn canvas_point(
    gesture: &gtk4::GestureDrag,
    fixed: &gtk4::Fixed,
    local: (f64, f64),
) -> Option<(f64, f64)> {
    let widget = gesture.widget()?;
    widget
        .compute_point(
            fixed,
            &gtk4::graphene::Point::new(local.0 as f32, local.1 as f32),
        )
        .map(|point| (point.x() as f64, point.y() as f64))
}

/// How far the pointer has moved, in world units, since the drag began.
/// `start_pointer` is `canvas_point` of the gesture's start point, captured
/// once in the `drag-begin` handler.
///
/// Why this isn't just `offset / zoom`: a `GestureDrag`'s `offset_x`/
/// `offset_y` are expressed in the coordinate space of the widget the
/// gesture is attached to, and GTK re-translates the pointer through that
/// widget's *current* transform on every event. Every gesture here is
/// attached to chrome inside a card whose transform the handler changes on
/// every tick, so the card's own displacement feeds straight back into the
/// reported offset: with displacement `d` applied and the pointer `m` from
/// where it started, GTK reports `offset = m - d`, so assigning `d = offset`
/// settles at `d = m / 2` — the card tracks at half the cursor's speed. The
/// same recurrence `d_n = m_n - d_(n-1)` has gain -1, so it never damps:
/// every pointer-sampling irregularity adds a non-decaying alternating
/// wobble, which is the jitter that grew the further a card was dragged.
///
/// Mapping both the start point and the current point into the canvas
/// `Fixed`'s space cancels the card's displacement exactly and leaves the
/// true pointer movement. `Fixed` is the right reference because it never
/// moves — pan and zoom only change its *children's* transforms, which is
/// why `Canvas`'s own pan gesture (attached to `fixed` itself) never had
/// this problem. It also makes the `/ zoom` correct: `Fixed`-space units are
/// screen pixels, whereas the raw gesture offsets were already in the card's
/// own zoom-scaled space and so were being divided by zoom a second time.
fn world_drag_delta(
    gesture: &gtk4::GestureDrag,
    fixed: &gtk4::Fixed,
    start_pointer: (f64, f64),
    zoom: f64,
) -> Option<(f64, f64)> {
    let (start_x, start_y) = gesture.start_point()?;
    let (offset_x, offset_y) = gesture.offset()?;
    let now = canvas_point(gesture, fixed, (start_x + offset_x, start_y + offset_y))?;
    Some((
        (now.0 - start_pointer.0) / zoom,
        (now.1 - start_pointer.1) / zoom,
    ))
}

/// Wires a session card's drag-to-move (`node.drag_handle`), drag-to-resize
/// (`node.resize_handle`), and close button (`node.close_button`). Shared by
/// `restore` and `create_session`, same as `wire_link_controls`.
///
/// Both gestures call `gesture.set_state(Claimed)` in their `drag-begin`
/// handler. Without this, the same press would also bubble up to `Canvas`'s
/// own pan `GestureDrag` (attached to `fixed`, an ancestor of every node),
/// since GTK delivers an event to every interested controller along a
/// widget's ancestor chain during the bubble phase unless one of them claims
/// the event sequence — so without claiming, moving/resizing a card would
/// simultaneously pan the whole canvas underneath it.
fn wire_session_chrome(app: &Rc<RefCell<App>>, node: &SessionNode, id: Uuid) {
    node.close_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::close_session(&app, id)
    });

    // (world position at drag start, pointer position at drag start in the
    // canvas `Fixed`'s stationary coordinate space) — see `world_drag_delta`
    // for why the pointer's start must be captured in that frame.
    let move_start = Rc::new(RefCell::new(None));
    let drag = gtk4::GestureDrag::new();
    drag.connect_drag_begin({
        let app = Rc::clone(app);
        let move_start = Rc::clone(&move_start);
        move |gesture, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let app_ref = app.borrow();
            *move_start.borrow_mut() = match (
                app_ref.sessions.get(&id),
                canvas_point(gesture, &app_ref.canvas.fixed, (x, y)),
            ) {
                (Some(entry), Some(pointer)) => Some((entry.record.position, pointer)),
                _ => None,
            };
        }
    });
    drag.connect_drag_update({
        let app = Rc::clone(app);
        let container = node.container.clone();
        let move_start = Rc::clone(&move_start);
        move |gesture, _offset_x, _offset_y| {
            let Some((start_position, start_pointer)) = *move_start.borrow() else {
                return;
            };
            let new_position = {
                let app_ref = app.borrow();
                let zoom = app_ref.canvas.state.borrow().zoom;
                match world_drag_delta(gesture, &app_ref.canvas.fixed, start_pointer, zoom) {
                    Some((dx, dy)) => (start_position.0 + dx, start_position.1 + dy),
                    None => return,
                }
            };
            let mut app_mut = app.borrow_mut();
            app_mut.canvas.reposition_node(&container, new_position);
            if let Some(entry) = app_mut.sessions.get_mut(&id) {
                entry.record.position = new_position;
            }
        }
    });
    // Persisting is deliberately NOT done in `drag-update` above: that fires
    // on every single pointer-motion tick during the drag (potentially
    // hundreds of times a second), and `schedule_persist` cancels and
    // re-registers a GLib main-loop timeout source on every call — doing
    // that on the hottest possible path made dragging visibly stutter
    // instead of smoothly tracking the cursor. `Canvas`'s own pan-drag
    // (`canvas.rs`) never persists mid-gesture either, for the same reason;
    // this matches that proven-smooth pattern by only persisting once, here,
    // when the drag actually ends.
    drag.connect_drag_end({
        let app = Rc::clone(app);
        move |_gesture, _x, _y| App::schedule_persist(&app)
    });
    node.drag_handle.add_controller(drag);

    // Same shape as `move_start` above: resizing a card moves its own
    // bottom-right grip, so the gesture's raw offsets suffer the identical
    // feedback described in `world_drag_delta`.
    let resize_start = Rc::new(RefCell::new(None));
    let resize = gtk4::GestureDrag::new();
    resize.connect_drag_begin({
        let app = Rc::clone(app);
        let terminal = node.terminal.clone();
        let resize_start = Rc::clone(&resize_start);
        move |gesture, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let app_ref = app.borrow();
            *resize_start.borrow_mut() = match (
                app_ref.sessions.get(&id),
                canvas_point(gesture, &app_ref.canvas.fixed, (x, y)),
            ) {
                // The terminal's *real* allocation, not `record.size` — see
                // `allocated_size` for why the two drift and why starting
                // from the record made a shrunken card un-growable.
                (Some(entry), Some(pointer)) => Some((
                    allocated_size(&terminal).unwrap_or(entry.record.size),
                    pointer,
                )),
                _ => None,
            };
        }
    });
    resize.connect_drag_update({
        let app = Rc::clone(app);
        let resize_start = Rc::clone(&resize_start);
        move |gesture, _offset_x, _offset_y| {
            let Some((start_size, start_pointer)) = *resize_start.borrow() else {
                return;
            };
            let new_size = {
                let app_ref = app.borrow();
                let zoom = app_ref.canvas.state.borrow().zoom;
                match world_drag_delta(gesture, &app_ref.canvas.fixed, start_pointer, zoom) {
                    Some((dx, dy)) => (
                        (start_size.0 + dx).max(crate::node::MIN_NODE_WIDTH),
                        (start_size.1 + dy).max(crate::node::MIN_NODE_HEIGHT),
                    ),
                    None => return,
                }
            };
            let mut app_mut = app.borrow_mut();
            let Some(entry) = app_mut.sessions.get_mut(&id) else {
                return;
            };
            entry.record.size = new_size;
            // Only a size *request*. The PTY follows VTE's real grid from
            // `pump_output` instead — see `SessionNode::request_grid`.
            entry.node.request_grid(new_size.0, new_size.1);
        }
    });
    // Same reasoning as the move gesture above: persist once at drag-end,
    // not on every resize tick. The record is also snapped back to the card's
    // real size here, so what gets persisted is a size the card can actually
    // render — otherwise a card dragged below its title bar's minimum width
    // would save the smaller requested number and restore into the same
    // record-vs-reality mismatch described on `allocated_size`.
    resize.connect_drag_end({
        let app = Rc::clone(app);
        let terminal = node.terminal.clone();
        move |_gesture, _x, _y| {
            if let Some(size) = allocated_size(&terminal) {
                if let Some(entry) = app.borrow_mut().sessions.get_mut(&id) {
                    entry.record.size = size;
                }
            }
            App::schedule_persist(&app);
        }
    });
    node.resize_handle.add_controller(resize);
}

/// Note equivalent of `wire_session_chrome` — same drag-to-move/resize/close
/// wiring, against `app.notes` and `node.text_view` instead of
/// `app.sessions`/`node.terminal`.
fn wire_note_chrome(app: &Rc<RefCell<App>>, node: &NoteNode, id: Uuid) {
    node.close_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::close_note(&app, id)
    });

    let move_start = Rc::new(RefCell::new(None));
    let drag = gtk4::GestureDrag::new();
    drag.connect_drag_begin({
        let app = Rc::clone(app);
        let move_start = Rc::clone(&move_start);
        move |gesture, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let app_ref = app.borrow();
            *move_start.borrow_mut() = match (
                app_ref.notes.get(&id),
                canvas_point(gesture, &app_ref.canvas.fixed, (x, y)),
            ) {
                (Some(entry), Some(pointer)) => Some((entry.record.position, pointer)),
                _ => None,
            };
        }
    });
    drag.connect_drag_update({
        let app = Rc::clone(app);
        let container = node.container.clone();
        let move_start = Rc::clone(&move_start);
        move |gesture, _offset_x, _offset_y| {
            let Some((start_position, start_pointer)) = *move_start.borrow() else {
                return;
            };
            let new_position = {
                let app_ref = app.borrow();
                let zoom = app_ref.canvas.state.borrow().zoom;
                match world_drag_delta(gesture, &app_ref.canvas.fixed, start_pointer, zoom) {
                    Some((dx, dy)) => (start_position.0 + dx, start_position.1 + dy),
                    None => return,
                }
            };
            let mut app_mut = app.borrow_mut();
            app_mut.canvas.reposition_node(&container, new_position);
            if let Some(entry) = app_mut.notes.get_mut(&id) {
                entry.record.position = new_position;
            }
        }
    });
    // See the matching comment in `wire_session_chrome`: persisting on every
    // `drag-update` tick (rather than once here, at drag end) is what made
    // dragging visibly stutter instead of smoothly tracking the cursor.
    drag.connect_drag_end({
        let app = Rc::clone(app);
        move |_gesture, _x, _y| App::schedule_persist(&app)
    });
    node.drag_handle.add_controller(drag);

    let resize_start = Rc::new(RefCell::new(None));
    let resize = gtk4::GestureDrag::new();
    resize.connect_drag_begin({
        let app = Rc::clone(app);
        let text_view = node.text_view.clone();
        let resize_start = Rc::clone(&resize_start);
        move |gesture, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let app_ref = app.borrow();
            *resize_start.borrow_mut() = match (
                app_ref.notes.get(&id),
                canvas_point(gesture, &app_ref.canvas.fixed, (x, y)),
            ) {
                // Same reasoning as `wire_session_chrome`: anchor the drag to
                // the body's real allocation, not the stored record.
                (Some(entry), Some(pointer)) => Some((
                    allocated_size(&text_view).unwrap_or(entry.record.size),
                    pointer,
                )),
                _ => None,
            };
        }
    });
    resize.connect_drag_update({
        let app = Rc::clone(app);
        let text_view = node.text_view.clone();
        let resize_start = Rc::clone(&resize_start);
        move |gesture, _offset_x, _offset_y| {
            let Some((start_size, start_pointer)) = *resize_start.borrow() else {
                return;
            };
            let new_size = {
                let app_ref = app.borrow();
                let zoom = app_ref.canvas.state.borrow().zoom;
                match world_drag_delta(gesture, &app_ref.canvas.fixed, start_pointer, zoom) {
                    Some((dx, dy)) => (
                        (start_size.0 + dx).max(crate::node::MIN_NODE_WIDTH),
                        (start_size.1 + dy).max(crate::node::MIN_NODE_HEIGHT),
                    ),
                    None => return,
                }
            };
            text_view.set_size_request(new_size.0 as i32, new_size.1 as i32);
            if let Some(entry) = app.borrow_mut().notes.get_mut(&id) {
                entry.record.size = new_size;
            }
        }
    });
    // See `wire_session_chrome`'s matching handler: persist once, and snap
    // the record to the body's real size.
    resize.connect_drag_end({
        let app = Rc::clone(app);
        let text_view = node.text_view.clone();
        move |_gesture, _x, _y| {
            if let Some(size) = allocated_size(&text_view) {
                if let Some(entry) = app.borrow_mut().notes.get_mut(&id) {
                    entry.record.size = size;
                }
            }
            App::schedule_persist(&app);
        }
    });
    node.resize_handle.add_controller(resize);
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
fn wire_link_controls(app: &Rc<RefCell<App>>, node: &SessionNode, id: Uuid, toast_overlay: &adw::ToastOverlay) {
    node.link_button.connect_clicked({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        move |_| {
            App::start_link(&app, id);
            toast_overlay.add_toast(adw::Toast::new(
                "link mode: click another session's terminal to send this one's output into it",
            ));
        }
    });

    // `switch_agent` kills the old process as its first, unconditional step,
    // before any of the fallible work (summarize, account setup, spawning
    // the new process) runs — so a failure here can leave the card's process
    // dead with no other signal. Surface it as a toast rather than swallowing
    // it, matching `App::restore`/the new-session dialog's error handling.
    node.handoff_button.connect_clicked({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        move |_| {
            if let Err(error) = App::switch_agent(&app, id) {
                toast_overlay.add_toast(adw::Toast::new(&error.to_string()));
            }
        }
    });

    let click = gtk4::GestureClick::new();
    click.connect_pressed({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        move |_gesture, _n_press, _x, _y| {
            if let Some(message) = App::complete_link_if_pending(&app, id) {
                toast_overlay.add_toast(adw::Toast::new(&message));
            }
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
                size: (720.0, 504.0),
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
                size: (720.0, 504.0),
            };
            Ok((launch, record))
        }
    }
}
