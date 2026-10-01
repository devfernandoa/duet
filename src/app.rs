use crate::account::AccountStore;
use crate::agent::{Agent, Launch, LaunchRequest};
use crate::canvas::Canvas;
use crate::handoff::{summarize_claude, summarize_codex};
use crate::node::{NoteNode, SessionNode};
use crate::role::Role;
use crate::session::Session;
use crate::store::{
    CanvasRecord, LinkRecord, SessionRecord, StickyNoteRecord, Store, WorkspaceRecord,
};
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
    /// The *active* workspace's identity. Its sessions/notes/links/canvas are
    /// the live state below (`sessions`, `notes`, `links`, `canvas`'s own pan/
    /// zoom) — only one workspace is ever loaded into real GTK widgets and
    /// PTYs at a time.
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub workspace_root: PathBuf,
    /// Every OTHER workspace's full persisted state: not live (no widgets, no
    /// PTYs), just the records needed to spawn it back in if the user
    /// switches to it. `switch_workspace`/`create_workspace`/
    /// `delete_workspace` are the only places that move a workspace between
    /// "this" (live) and an entry here (dormant).
    pub inactive_workspaces: Vec<WorkspaceRecord>,
    /// User-created roles, global across every workspace (unlike
    /// `sessions`/`notes`/`links` below). Built-in roles come from
    /// `role::builtin_roles` and are never stored here.
    pub custom_roles: Vec<Role>,
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
            // Overwritten by `restore` if the store already has workspaces;
            // kept as-is (empty) on a brand-new install, so the very first
            // save still writes one real, named workspace rather than a
            // special-cased empty shape.
            workspace_id: Uuid::new_v4(),
            workspace_name: "Default".to_string(),
            workspace_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            inactive_workspaces: Vec::new(),
            custom_roles: Vec::new(),
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

    /// Loads the store, makes the saved `active_workspace` (or the first
    /// workspace, if that id isn't found) the live one, and spawns its
    /// sessions/notes into the canvas — "reopen the most recently used
    /// workspace" is just restoring whatever was active when last saved.
    /// Every other workspace's records are kept dormant in
    /// `inactive_workspaces` until switched to. A brand-new install (no store
    /// file yet, so `saved.workspaces` is empty) keeps the placeholder
    /// workspace `App::new` already set up, rather than special-casing "no
    /// workspace" anywhere else. `toast_overlay` is threaded down into
    /// `spawn_workspace_contents` so a later failed handoff on a restored
    /// session can surface its own toast.
    pub fn restore(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) -> Vec<String> {
        let (saved, load_warning) = {
            let app_ref = app.borrow();
            Store::load_with_warning(&app_ref.store_path)
        };
        let mut errors: Vec<String> = load_warning.into_iter().collect();
        app.borrow_mut().custom_roles = saved.custom_roles;

        if saved.workspaces.is_empty() {
            return errors;
        }

        let mut workspaces = saved.workspaces;
        let active_index = saved
            .active_workspace
            .and_then(|id| workspaces.iter().position(|w| w.id == id))
            .unwrap_or(0);
        let active = workspaces.remove(active_index);

        app.borrow_mut().inactive_workspaces = workspaces;
        App::activate_workspace(app, &active);

        errors.extend(spawn_workspace_contents(
            app,
            active.sessions,
            active.notes,
            active.links,
            toast_overlay,
        ));
        errors
    }

    pub fn persist(&self) -> std::io::Result<()> {
        let mut workspaces = self.inactive_workspaces.clone();
        workspaces.push(self.snapshot_active_workspace());
        let store = Store {
            workspaces,
            active_workspace: Some(self.workspace_id),
            custom_roles: self.custom_roles.clone(),
        };
        store.save(&self.store_path)
    }

    /// Every role available to assign to a session: built-ins first, then
    /// user-created ones.
    pub fn roles(&self) -> Vec<Role> {
        let mut roles = crate::role::builtin_roles();
        roles.extend(self.custom_roles.iter().cloned());
        roles
    }

    pub fn find_role(&self, id: Uuid) -> Option<Role> {
        self.roles().into_iter().find(|role| role.id == id)
    }

    /// The instructions text for a session's assigned role, if it has one —
    /// what a fresh (or handed-off) launch prefixes onto its prompt. `None`
    /// both when no role is assigned and when the assigned role no longer
    /// exists (a custom role deleted out from under an old record, say).
    pub fn role_instructions(&self, role_id: Option<Uuid>) -> Option<String> {
        role_id
            .and_then(|id| self.find_role(id))
            .map(|role| role.instructions)
    }

    /// Creates a user-defined role and persists it.
    pub fn create_role(
        app: &Rc<RefCell<App>>,
        name: String,
        instructions: String,
        icon: Option<String>,
        accent: Option<String>,
    ) -> anyhow::Result<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("give this role a name");
        }
        app.borrow_mut().custom_roles.push(Role {
            id: Uuid::new_v4(),
            name,
            instructions,
            icon,
            accent,
        });
        app.borrow().persist()?;
        Ok(())
    }

    /// Updates a user-defined role in place, then refreshes the role badge
    /// on every live session currently assigned to it (name/icon/accent may
    /// have changed). Built-in roles have no id in `custom_roles`, so this
    /// can never touch one.
    pub fn update_role(
        app: &Rc<RefCell<App>>,
        id: Uuid,
        name: String,
        instructions: String,
        icon: Option<String>,
        accent: Option<String>,
    ) -> anyhow::Result<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("give this role a name");
        }
        {
            let mut app_mut = app.borrow_mut();
            let role = app_mut
                .custom_roles
                .iter_mut()
                .find(|role| role.id == id)
                .context("role not found")?;
            role.name = name.clone();
            role.instructions = instructions;
            role.icon = icon.clone();
            role.accent = accent.clone();
            for entry in app_mut.sessions.values_mut() {
                if entry.record.role_id == Some(id) {
                    entry
                        .node
                        .set_role(Some(&name), icon.as_deref(), accent.as_deref());
                }
            }
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// Deletes a custom role, unassigning it (rather than killing anything)
    /// from every session that referenced it — losing a role is cosmetic/
    /// instructional, not a login identity a running process depends on the
    /// way an account's config directory is.
    pub fn delete_role(app: &Rc<RefCell<App>>, id: Uuid) -> anyhow::Result<()> {
        {
            let mut app_mut = app.borrow_mut();
            app_mut.custom_roles.retain(|role| role.id != id);
            for entry in app_mut.sessions.values_mut() {
                if entry.record.role_id == Some(id) {
                    entry.record.role_id = None;
                    entry.node.set_role(None, None, None);
                }
            }
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// The active workspace's current live state, as a `WorkspaceRecord` —
    /// what gets written to disk for it, and what gets tucked into
    /// `inactive_workspaces` when switching away from it.
    fn snapshot_active_workspace(&self) -> WorkspaceRecord {
        let state = self.canvas.state.borrow();
        WorkspaceRecord {
            id: self.workspace_id,
            name: self.workspace_name.clone(),
            root_dir: self.workspace_root.clone(),
            sessions: self
                .sessions
                .values()
                .map(|entry| entry.record.clone())
                .collect(),
            notes: self
                .notes
                .values()
                .map(|entry| entry.record.clone())
                .collect(),
            links: self.links.clone(),
            canvas: CanvasRecord {
                zoom: state.zoom,
                pan: state.pan,
            },
        }
    }

    /// Lists every workspace (the active one included) as `(id, name)`,
    /// sorted by name — a stable order independent of which one happens to
    /// be active, so "the first several workspaces" (for the Ctrl+1..9
    /// shortcuts) means the same thing from one switch to the next.
    pub fn workspace_list(&self) -> Vec<(Uuid, String)> {
        let mut list: Vec<(Uuid, String)> =
            std::iter::once((self.workspace_id, self.workspace_name.clone()))
                .chain(
                    self.inactive_workspaces
                        .iter()
                        .map(|w| (w.id, w.name.clone())),
                )
                .collect();
        list.sort_by_key(|(_, name)| name.to_lowercase());
        list
    }

    /// Kills every live session's process and clears the canvas of the
    /// active workspace's nodes, without saving its state anywhere — callers
    /// are responsible for snapshotting first if the workspace should come
    /// back later (`switch_workspace`, `create_workspace`) or deliberately
    /// skip that if it's being deleted (`delete_workspace`).
    fn teardown_active_workspace(app: &Rc<RefCell<App>>) {
        let (session_entries, note_entries) = {
            let mut app_mut = app.borrow_mut();
            (
                app_mut.sessions.drain().collect::<Vec<_>>(),
                app_mut.notes.drain().collect::<Vec<_>>(),
            )
        };
        let mut app_mut = app.borrow_mut();
        for (_, mut entry) in session_entries {
            entry.session.kill();
            app_mut.canvas.remove_node(&entry.node.container);
        }
        for (_, entry) in note_entries {
            app_mut.canvas.remove_node(&entry.node.container);
        }
        app_mut.links.clear();
        app_mut.pending_link_source = None;
        app_mut.selected_link = None;
    }

    /// Replaces or inserts `record` into `inactive_workspaces` by id — the
    /// "put this workspace back on the shelf" half of switching away from it.
    fn stash_workspace(app: &Rc<RefCell<App>>, record: WorkspaceRecord) {
        let mut app_mut = app.borrow_mut();
        if let Some(existing) = app_mut
            .inactive_workspaces
            .iter_mut()
            .find(|w| w.id == record.id)
        {
            *existing = record;
        } else {
            app_mut.inactive_workspaces.push(record);
        }
    }

    /// Applies a `WorkspaceRecord`'s identity and canvas pan/zoom to `App`
    /// and its live `Canvas` state — but does NOT spawn its sessions/notes;
    /// that's `spawn_workspace_contents`'s job, called separately right after
    /// this by every caller. Factored out because "which workspace is
    /// active" and "what its canvas looks like" must always change together,
    /// and previously changed together by copy-pasted blocks across
    /// `restore`, `switch_workspace`, and `delete_workspace`.
    fn activate_workspace(app: &Rc<RefCell<App>>, record: &WorkspaceRecord) {
        let mut app_mut = app.borrow_mut();
        app_mut.workspace_id = record.id;
        app_mut.workspace_name = record.name.clone();
        app_mut.workspace_root = record.root_dir.clone();
        let mut state = app_mut.canvas.state.borrow_mut();
        state.zoom = record.canvas.zoom;
        state.pan = record.canvas.pan;
    }

    /// Makes `target_id` the active workspace: pulls its record out of
    /// `inactive_workspaces` *first* (so an unknown/stale `target_id` leaves
    /// the current workspace completely untouched rather than torn down for
    /// a switch that can't complete), then snapshots and stashes the current
    /// workspace and spawns the target's sessions/notes into the now-cleared
    /// canvas. A no-op if `target_id` is already active. Persists on success.
    pub fn switch_workspace(
        app: &Rc<RefCell<App>>,
        target_id: Uuid,
        toast_overlay: &adw::ToastOverlay,
    ) -> Vec<String> {
        if app.borrow().workspace_id == target_id {
            return Vec::new();
        }

        let target = {
            let mut app_mut = app.borrow_mut();
            let position = app_mut
                .inactive_workspaces
                .iter()
                .position(|w| w.id == target_id);
            position.map(|index| app_mut.inactive_workspaces.remove(index))
        };
        let Some(target) = target else {
            return vec!["that workspace no longer exists".to_string()];
        };

        let outgoing = app.borrow().snapshot_active_workspace();
        App::teardown_active_workspace(app);
        App::stash_workspace(app, outgoing);

        App::activate_workspace(app, &target);
        let errors = spawn_workspace_contents(
            app,
            target.sessions,
            target.notes,
            target.links,
            toast_overlay,
        );
        let _ = app.borrow().persist();
        errors
    }

    /// Creates a brand-new, empty workspace and switches to it immediately —
    /// there is no separate "create" state a user would ever see with nothing
    /// open. Rejects an empty or duplicate name, the same rule `create_session`
    /// already applies to session names.
    pub fn create_workspace(
        app: &Rc<RefCell<App>>,
        name: String,
        root_dir: PathBuf,
    ) -> anyhow::Result<Uuid> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("give this workspace a name");
        }
        {
            let app_ref = app.borrow();
            let duplicate = app_ref.workspace_name == name
                || app_ref.inactive_workspaces.iter().any(|w| w.name == name);
            if duplicate {
                anyhow::bail!("a workspace named '{name}' already exists");
            }
        }

        let outgoing = app.borrow().snapshot_active_workspace();
        App::teardown_active_workspace(app);
        App::stash_workspace(app, outgoing);

        let new_id = Uuid::new_v4();
        {
            let mut app_mut = app.borrow_mut();
            app_mut.workspace_id = new_id;
            app_mut.workspace_name = name;
            app_mut.workspace_root = root_dir;
        }
        {
            let app_ref = app.borrow();
            *app_ref.canvas.state.borrow_mut() = crate::canvas::CanvasState::new();
        }
        app.borrow().persist()?;
        Ok(new_id)
    }

    /// Renames a workspace, active or dormant, enforcing the same
    /// non-empty/unique rule as `create_workspace`.
    pub fn rename_workspace(
        app: &Rc<RefCell<App>>,
        id: Uuid,
        new_name: &str,
    ) -> anyhow::Result<()> {
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() {
            anyhow::bail!("give this workspace a name");
        }
        {
            let mut app_mut = app.borrow_mut();
            let duplicate = (app_mut.workspace_id != id && app_mut.workspace_name == new_name)
                || app_mut
                    .inactive_workspaces
                    .iter()
                    .any(|w| w.id != id && w.name == new_name);
            if duplicate {
                anyhow::bail!("a workspace named '{new_name}' already exists");
            }
            if app_mut.workspace_id == id {
                app_mut.workspace_name = new_name;
            } else if let Some(workspace) =
                app_mut.inactive_workspaces.iter_mut().find(|w| w.id == id)
            {
                workspace.name = new_name;
            } else {
                anyhow::bail!("workspace not found");
            }
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// Deletes a workspace — killing its sessions first if it's the active
    /// one — after confirming at least one other workspace exists to fall
    /// back to (switching to the first remaining one, by the same stable
    /// order as `workspace_list`). The caller (a UI confirmation dialog) is
    /// responsible for asking the user first; this performs the deletion
    /// unconditionally.
    pub fn delete_workspace(
        app: &Rc<RefCell<App>>,
        id: Uuid,
        toast_overlay: &adw::ToastOverlay,
    ) -> anyhow::Result<()> {
        let is_active = app.borrow().workspace_id == id;
        if app.borrow().inactive_workspaces.is_empty() {
            anyhow::bail!("can't delete the only workspace");
        }

        if is_active {
            App::teardown_active_workspace(app);
            let next = {
                let mut app_mut = app.borrow_mut();
                // The same stable, active-independent order as
                // `workspace_list`, so "fall back to the first remaining
                // workspace" means the same thing a user would see in the
                // switcher.
                let index = app_mut
                    .inactive_workspaces
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, w)| w.name.to_lowercase())
                    .map(|(index, _)| index)
                    .context("just checked inactive_workspaces is non-empty")?;
                app_mut.inactive_workspaces.remove(index)
            };
            App::activate_workspace(app, &next);
            spawn_workspace_contents(app, next.sessions, next.notes, next.links, toast_overlay);
        } else {
            app.borrow_mut().inactive_workspaces.retain(|w| w.id != id);
        }
        app.borrow().persist()?;
        Ok(())
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
        role_id: Option<Uuid>,
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
            if app_ref
                .sessions
                .values()
                .any(|entry| entry.record.name == name)
            {
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
                role_id,
                viewport_center_world,
            )?
        };
        let session = Session::spawn(cwd, launch)?;
        let node = SessionNode::new(&name);
        node.request_grid(record.size.0, record.size.1);
        apply_role_badge(&app.borrow(), &node, role_id);
        let id = record.id;
        {
            let app_ref = app.borrow();
            app_ref
                .canvas
                .add_node(&node.container, viewport_center_world);
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
        app.borrow_mut()
            .notes
            .insert(id, NoteEntry { record, node });
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
            && !app
                .links
                .iter()
                .any(|l| l.source == source && l.target == target)
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
        app.links
            .retain(|l| !(l.source == source && l.target == target));
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
            let app_ref = app.borrow();
            app_ref
                .sessions
                .get(&id)
                .context("session not found")?
                .record
                .clone()
        };
        // Checked before anything is torn down: a session whose agent has no
        // handoff support (every provider besides Claude/Codex) should be
        // left running untouched, not killed for a switch that can't
        // meaningfully complete.
        if !record.agent.supports_handoff() {
            anyhow::bail!(
                "{} sessions don't support handing off to another agent",
                record.agent.display_name()
            );
        }
        if let Some(entry) = app.borrow_mut().sessions.get_mut(&id) {
            entry.session.kill();
        }

        let is_claude = matches!(record.agent, Agent::Claude);
        let summary_result = if is_claude {
            let session_id = record
                .claude_session_id
                .context("session has no Claude session to summarize")?;
            let config_dir = record
                .claude_account
                .as_ref()
                .map(|a| app.borrow().accounts.config_dir(a));
            summarize_claude(session_id, config_dir.as_deref(), &record.cwd)
        } else {
            summarize_codex(&record.cwd)
        };
        let summary = match summary_result {
            Ok(summary) => summary,
            Err(_) => "The previous agent session could not be recovered. Start by inspecting the working directory and continue from there.".to_string(),
        };

        let to = if is_claude {
            Agent::Codex
        } else {
            Agent::Claude
        };
        let (launch, updated_record) = {
            let app_ref = app.borrow();
            let initial_prompt =
                with_role_instructions(app_ref.role_instructions(record.role_id), Some(&summary));
            if matches!(to, Agent::Claude) {
                let account = crate::account::DEFAULT_ACCOUNT.to_string();
                let config_dir = app_ref.accounts.ensure(&account)?;
                let session_id = Uuid::new_v4();
                let launch = to.launch(LaunchRequest {
                    initial_prompt: initial_prompt.as_deref(),
                    claude_session_id: Some(session_id),
                    claude_config_dir: Some(&config_dir),
                    ..Default::default()
                });
                let mut updated = record.clone();
                updated.agent = Agent::Claude;
                updated.claude_session_id = Some(session_id);
                updated.claude_account = Some(account);
                (launch, updated)
            } else {
                let launch = to.launch(LaunchRequest {
                    initial_prompt: initial_prompt.as_deref(),
                    ..Default::default()
                });
                let mut updated = record.clone();
                updated.agent = Agent::Codex;
                updated.claude_session_id = None;
                updated.claude_account = None;
                (launch, updated)
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

/// Milestone-1 seam for generic canvas nodes: the part of `SessionEntry`/
/// `NoteEntry` that `wire_node_chrome` needs, so a card's drag-to-move/
/// resize/close wiring is written once instead of once per node kind. A
/// future node kind (file tree, browser, ...) implements this instead of
/// copying a ~140-line function; it does not touch how sessions or notes are
/// stored, spawned, or persisted, which stay genuinely different per kind
/// (a session owns a `Session`/PTY; a note does not) and are left alone.
trait CardEntry {
    fn position(&self) -> (f64, f64);
    fn set_position(&mut self, position: (f64, f64));
    fn size(&self) -> (f64, f64);
    fn set_size(&mut self, size: (f64, f64));
    /// Applies a size mid-drag. A note just requests a widget size; a session
    /// additionally keeps the VTE character grid in step — see
    /// `SessionNode::request_grid`.
    fn apply_resize(&self, size: (f64, f64));
}

impl CardEntry for SessionEntry {
    fn position(&self) -> (f64, f64) {
        self.record.position
    }
    fn set_position(&mut self, position: (f64, f64)) {
        self.record.position = position;
    }
    fn size(&self) -> (f64, f64) {
        self.record.size
    }
    fn set_size(&mut self, size: (f64, f64)) {
        self.record.size = size;
    }
    fn apply_resize(&self, size: (f64, f64)) {
        self.node.request_grid(size.0, size.1);
    }
}

impl CardEntry for NoteEntry {
    fn position(&self) -> (f64, f64) {
        self.record.position
    }
    fn set_position(&mut self, position: (f64, f64)) {
        self.record.position = position;
    }
    fn size(&self) -> (f64, f64) {
        self.record.size
    }
    fn set_size(&mut self, size: (f64, f64)) {
        self.record.size = size;
    }
    fn apply_resize(&self, size: (f64, f64)) {
        self.node
            .text_view
            .set_size_request(size.0 as i32, size.1 as i32);
    }
}

/// Wires a card's drag-to-move (`drag_handle`), drag-to-resize
/// (`resize_handle`), and `close_button` — the chrome shared by every node
/// kind on the canvas. `entries` and `on_close` are the only places this
/// function knows which kind it's wiring: a plain field accessor
/// (`|app| &mut app.sessions`-shaped, written as a free function so it has no
/// captures) and `App::close_session`/`App::close_note`. `resizable_widget`
/// is the widget whose *real* GTK allocation a resize should anchor to and
/// snap back to (`allocated_size`'s doc explains why that differs from the
/// stored record).
///
/// Both gestures call `gesture.set_state(Claimed)` in their `drag-begin`
/// handler. Without this, the same press would also bubble up to `Canvas`'s
/// own pan `GestureDrag` (attached to `fixed`, an ancestor of every node),
/// since GTK delivers an event to every interested controller along a
/// widget's ancestor chain during the bubble phase unless one of them claims
/// the event sequence — so without claiming, moving/resizing a card would
/// simultaneously pan the whole canvas underneath it.
fn wire_node_chrome<T: CardEntry + 'static>(
    app: &Rc<RefCell<App>>,
    container: &gtk4::Box,
    drag_handle: &gtk4::Box,
    resize_handle: &gtk4::Box,
    close_button: &gtk4::Button,
    resizable_widget: gtk4::Widget,
    id: Uuid,
    entries: fn(&mut App) -> &mut HashMap<Uuid, T>,
    on_close: fn(&Rc<RefCell<App>>, Uuid),
) {
    close_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| on_close(&app, id)
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
            let mut app_mut = app.borrow_mut();
            let pointer = canvas_point(gesture, &app_mut.canvas.fixed, (x, y));
            let position = entries(&mut app_mut).get(&id).map(CardEntry::position);
            *move_start.borrow_mut() = match (position, pointer) {
                (Some(position), Some(pointer)) => Some((position, pointer)),
                _ => None,
            };
        }
    });
    drag.connect_drag_update({
        let app = Rc::clone(app);
        let container = container.clone();
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
            if let Some(entry) = entries(&mut app_mut).get_mut(&id) {
                entry.set_position(new_position);
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
    drag_handle.add_controller(drag);

    // Same shape as `move_start` above: resizing a card moves its own
    // bottom-right grip, so the gesture's raw offsets suffer the identical
    // feedback described in `world_drag_delta`.
    let resize_start = Rc::new(RefCell::new(None));
    let resize = gtk4::GestureDrag::new();
    resize.connect_drag_begin({
        let app = Rc::clone(app);
        let resizable_widget = resizable_widget.clone();
        let resize_start = Rc::clone(&resize_start);
        move |gesture, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let mut app_mut = app.borrow_mut();
            let pointer = canvas_point(gesture, &app_mut.canvas.fixed, (x, y));
            // The widget's *real* allocation, not the stored record — see
            // `allocated_size` for why the two drift and why starting from
            // the record made a shrunken card un-growable.
            let fallback_size = entries(&mut app_mut).get(&id).map(CardEntry::size);
            *resize_start.borrow_mut() = match (fallback_size, pointer) {
                (Some(fallback_size), Some(pointer)) => Some((
                    allocated_size(&resizable_widget).unwrap_or(fallback_size),
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
            let Some(entry) = entries(&mut app_mut).get_mut(&id) else {
                return;
            };
            entry.set_size(new_size);
            entry.apply_resize(new_size);
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
        let resizable_widget = resizable_widget.clone();
        move |_gesture, _x, _y| {
            if let Some(size) = allocated_size(&resizable_widget) {
                let mut app_mut = app.borrow_mut();
                if let Some(entry) = entries(&mut app_mut).get_mut(&id) {
                    entry.set_size(size);
                }
            }
            App::schedule_persist(&app);
        }
    });
    resize_handle.add_controller(resize);
}

fn sessions_map(app: &mut App) -> &mut HashMap<Uuid, SessionEntry> {
    &mut app.sessions
}

fn notes_map(app: &mut App) -> &mut HashMap<Uuid, NoteEntry> {
    &mut app.notes
}

/// Wires a session card's move/resize/close chrome via `wire_node_chrome`.
/// Shared by `restore` and `create_session`, same as `wire_link_controls`.
fn wire_session_chrome(app: &Rc<RefCell<App>>, node: &SessionNode, id: Uuid) {
    wire_node_chrome(
        app,
        &node.container,
        &node.drag_handle,
        &node.resize_handle,
        &node.close_button,
        node.terminal.clone().upcast(),
        id,
        sessions_map,
        App::close_session,
    );
}

/// Note equivalent of `wire_session_chrome`, via the same `wire_node_chrome`.
fn wire_note_chrome(app: &Rc<RefCell<App>>, node: &NoteNode, id: Uuid) {
    wire_node_chrome(
        app,
        &node.container,
        &node.drag_handle,
        &node.resize_handle,
        &node.close_button,
        node.text_view.clone().upcast(),
        id,
        notes_map,
        App::close_note,
    );
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
fn wire_link_controls(
    app: &Rc<RefCell<App>>,
    node: &SessionNode,
    id: Uuid,
    toast_overlay: &adw::ToastOverlay,
) {
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

    // `switch_agent` refuses up front (before killing anything) for a
    // provider with no handoff support, but once past that check it still
    // kills the old process before the rest of the fallible work (summarize,
    // account setup, spawning the new process) — so a failure after that
    // point can leave the card's process dead with no other signal. Surface
    // it as a toast rather than swallowing it, matching `App::restore`/the
    // new-session dialog's error handling.
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

/// Spawns one `Session` + `SessionNode` per `sessions` record and one
/// `NoteNode` per `notes` record into the (already-cleared) canvas, wiring
/// each exactly as `restore` always has, then sets `links` as the active
/// workspace's link list. Shared by every path that makes a workspace's
/// saved records live: the initial `restore`, and `switch_workspace`/
/// `delete_workspace` activating a different workspace. Spawn failures are
/// collected and returned rather than aborting the rest of the workspace.
fn spawn_workspace_contents(
    app: &Rc<RefCell<App>>,
    sessions: Vec<SessionRecord>,
    notes: Vec<StickyNoteRecord>,
    links: Vec<LinkRecord>,
    toast_overlay: &adw::ToastOverlay,
) -> Vec<String> {
    let mut errors = Vec::new();

    for record in sessions {
        let launch = {
            let app_ref = app.borrow();
            let claude = match resolve_claude_account(
                &app_ref,
                &record.agent,
                record.claude_account.clone(),
            ) {
                Ok(claude) => claude,
                Err(error) => {
                    errors.push(format!("couldn't restore {}: {error}", record.name));
                    continue;
                }
            };
            // Claude only resumes a prior conversation when this record
            // already pinned a session id; every other provider is simply
            // relaunched fresh (Codex's own `--last` happens to pick up its
            // most recent conversation regardless of this flag, consistent
            // with the pre-workspace behavior).
            let resume = match &record.agent {
                Agent::Claude => record.claude_session_id.is_some(),
                _ => true,
            };
            record.agent.launch(LaunchRequest {
                resume,
                claude_session_id: record.claude_session_id,
                claude_config_dir: claude.as_ref().map(|(_, dir)| dir.as_path()),
                ..Default::default()
            })
        };
        match Session::spawn(record.cwd.clone(), launch) {
            Ok(session) => {
                let node = SessionNode::new(&record.name);
                // Ask for the saved size; `pump_output` syncs the PTY to
                // whatever grid VTE actually ends up rendering.
                node.request_grid(record.size.0, record.size.1);
                apply_role_badge(&app.borrow(), &node, record.role_id);
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

    app.borrow_mut().links = links;

    for note_record in notes {
        let node = NoteNode::new(&note_record.text, &note_record.color);
        node.text_view
            .set_size_request(note_record.size.0 as i32, note_record.size.1 as i32);
        {
            let app_ref = app.borrow();
            app_ref
                .canvas
                .add_node(&node.container, note_record.position);
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
        app.borrow_mut().notes.insert(
            id,
            NoteEntry {
                record: note_record,
                node,
            },
        );
    }

    errors
}

/// Resolves a Claude account's isolated config directory for a Claude-kind
/// agent, falling back to the default account when none was picked; `None`
/// for every other agent, since `account.rs`'s isolation is Claude-only.
/// Returns the account name alongside the directory so the caller can
/// persist which account was actually used onto the session's record.
fn resolve_claude_account(
    app: &App,
    agent: &Agent,
    claude_account: Option<String>,
) -> anyhow::Result<Option<(String, PathBuf)>> {
    if !agent.supports_accounts() {
        return Ok(None);
    }
    let account = claude_account.unwrap_or_else(|| crate::account::DEFAULT_ACCOUNT.to_string());
    let dir = app.accounts.ensure(&account)?;
    Ok(Some((account, dir)))
}

/// Combines a role's instructions with whatever prompt text a launch already
/// has (a handoff summary, or nothing for a brand-new session) into one
/// `initial_prompt` — the single place role injection happens, reused by
/// every launch site instead of each one re-deciding how to fold a role in.
/// Goes through the same provider-agnostic `LaunchRequest::initial_prompt`
/// field `handoff.rs` already uses, so it needs no Claude-specific code.
fn with_role_instructions(role_instructions: Option<String>, base: Option<&str>) -> Option<String> {
    match (role_instructions, base) {
        (None, None) => None,
        (Some(role), None) => Some(role),
        (None, Some(base)) => Some(base.to_string()),
        (Some(role), Some(base)) => Some(format!("{role}\n\n{base}")),
    }
}

/// Looks up `role_id` and updates `node`'s title-bar badge to match —
/// shared by `create_session` and `spawn_workspace_contents` so a session's
/// assigned role is shown the same way whether it was just created or
/// restored.
fn apply_role_badge(app: &App, node: &SessionNode, role_id: Option<Uuid>) {
    let role = role_id.and_then(|id| app.find_role(id));
    node.set_role(
        role.as_ref().map(|role| role.name.as_str()),
        role.as_ref().and_then(|role| role.icon.as_deref()),
        role.as_ref().and_then(|role| role.accent.as_deref()),
    );
}

fn build_launch_and_record(
    app: &App,
    name: &str,
    cwd: &Path,
    agent: Agent,
    claude_account: Option<String>,
    role_id: Option<Uuid>,
    position: (f64, f64),
) -> anyhow::Result<(Launch, SessionRecord)> {
    if let Agent::Custom { program, .. } = &agent {
        if program.trim().is_empty() {
            anyhow::bail!("give the custom command a program to run");
        }
    }
    let claude = resolve_claude_account(app, &agent, claude_account)?;
    let claude_session_id = claude.is_some().then(Uuid::new_v4);
    let initial_prompt = with_role_instructions(app.role_instructions(role_id), None);
    let launch = agent.launch(LaunchRequest {
        claude_session_id,
        claude_config_dir: claude.as_ref().map(|(_, dir)| dir.as_path()),
        initial_prompt: initial_prompt.as_deref(),
        ..Default::default()
    });
    let record = SessionRecord {
        id: Uuid::new_v4(),
        name: name.to_string(),
        cwd: cwd.to_path_buf(),
        claude_account: claude.map(|(account, _)| account),
        claude_session_id,
        agent,
        role_id,
        position,
        size: (720.0, 504.0),
    };
    Ok((launch, record))
}
