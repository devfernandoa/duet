use crate::account::AccountStore;
use crate::agent::{Agent, Launch, LaunchRequest, with_session_env};
use crate::canvas::{
    Canvas, NodeGeometry, TITLE_BAR_HEIGHT, allocated_size, canvas_point, card_center, card_edge,
    card_intersection, card_rect, card_vertical_edge, world_drag_delta,
};
use crate::environment;
use crate::handoff::{summarize_claude, summarize_codex};
use crate::layout;
use crate::message::{AgentMessage, AgentSummary, DeliveryStatus, LinkSummary, now_epoch_secs};
use crate::model::{
    EdgeRecord, EnvironmentKind, FloorRef, GroupPayload, NodeKind, NodeRecord, NotePayload,
    NoteViewMode, TerminalPayload, TextPayload,
};
use crate::node::{NoteNode, PlaceholderNode, SessionNode, TextNode};
use crate::role::{Role, with_role_instructions};
use crate::runtime::{AgentActivity, SessionRuntime};
use crate::store::{CanvasRecord, Store, WorkspaceRecord};
use anyhow::Context;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use uuid::Uuid;

/// One edge's world-space endpoints, as `edge_lines` resolves it: the edge
/// itself, then its `from`/`to` points.
type EdgeEndpoints = (EdgeRecord, (f64, f64), (f64, f64));

/// `wire_node_chrome`'s drag-to-move state: every selected node's position
/// at drag start, plus the pointer's own start position (see
/// `world_drag_delta`'s doc for why the pointer start must be captured in
/// the canvas `Fixed`'s coordinate space).
type MoveStart = Rc<RefCell<Option<(HashMap<Uuid, (f64, f64)>, (f64, f64))>>>;

/// `wire_node_chrome`'s drag-to-resize state: the node's size and the
/// pointer's position, both at drag start.
type ResizeStart = Rc<RefCell<Option<((f64, f64), (f64, f64))>>>;

/// One node's move, as `CanvasCommand::MoveNodes` records it for undo/redo:
/// the node's id, its position before, and its position after.
type NodeMove = (Uuid, (f64, f64), (f64, f64));

/// How long to wait after the last edit before writing the store to disk.
/// Keeps rapid-fire events (e.g. every keystroke in a sticky note) from each
/// triggering their own synchronous `File::create` + `write_all` +
/// `sync_all` + `rename`.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

/// Caps `App::messages` so a long-running duet with chatty agents doesn't
/// grow the log unboundedly in memory.
const MESSAGE_LOG_LIMIT: usize = 200;

/// How long to wait after writing a message's text before writing the
/// trailing `\r` that submits it. Needed because Claude/Codex's own input
/// box (unlike a plain shell's line discipline) distinguishes a human
/// keystroke from a paste by whether a newline arrived in the same burst of
/// bytes as the rest of the line: written in one `write_input` call, the
/// trailing `\r` reads exactly like a pasted newline and is inserted into
/// the input instead of submitting it. A separate write, after the PTY
/// reader has had a real chance to drain the first one, reads as a
/// distinct keystroke instead.
const MESSAGE_SUBMIT_DELAY: Duration = Duration::from_millis(120);

/// Offset applied to a duplicated or pasted node so it doesn't land exactly
/// on top of its source.
const DUPLICATE_OFFSET: (f64, f64) = (32.0, 32.0);

/// The GTK widget half of a node, one variant per `NodeKind`. Kept separate
/// from `NodeRecord` (the persisted half) the same way `SessionNode` always
/// was — this module is the only place that matches on both the kind of
/// record and the kind of widget at once.
pub enum NodeWidget {
    Terminal(SessionNode),
    Note(NoteNode),
    Text(TextNode),
    /// `FileTree`/`Portal`/`Drawing`/`Group` all render as the same
    /// placeholder today — see `model.rs`'s doc comment for why.
    Placeholder(PlaceholderNode),
}

impl NodeWidget {
    fn container(&self) -> &gtk4::Box {
        match self {
            NodeWidget::Terminal(node) => &node.container,
            NodeWidget::Note(node) => &node.container,
            NodeWidget::Text(node) => &node.container,
            NodeWidget::Placeholder(node) => &node.container,
        }
    }

    fn drag_handle(&self) -> &gtk4::Box {
        match self {
            NodeWidget::Terminal(node) => &node.drag_handle,
            NodeWidget::Note(node) => &node.drag_handle,
            NodeWidget::Text(node) => &node.drag_handle,
            NodeWidget::Placeholder(node) => &node.drag_handle,
        }
    }

    fn resize_handle(&self) -> &gtk4::Box {
        match self {
            NodeWidget::Terminal(node) => &node.resize_handle,
            NodeWidget::Note(node) => &node.resize_handle,
            NodeWidget::Text(node) => &node.resize_handle,
            NodeWidget::Placeholder(node) => &node.resize_handle,
        }
    }

    fn close_button(&self) -> &gtk4::Button {
        match self {
            NodeWidget::Terminal(node) => &node.close_button,
            NodeWidget::Note(node) => &node.close_button,
            NodeWidget::Text(node) => &node.close_button,
            NodeWidget::Placeholder(node) => &node.close_button,
        }
    }

    /// The widget whose *real* GTK allocation a resize drag should anchor to
    /// — see `allocated_size`'s doc for why that differs from the stored
    /// record.
    fn resizable_widget(&self) -> gtk4::Widget {
        match self {
            NodeWidget::Terminal(node) => node.terminal.clone().upcast(),
            NodeWidget::Note(node) => node.container.clone().upcast(),
            NodeWidget::Text(node) => node.text_view.clone().upcast(),
            NodeWidget::Placeholder(node) => node.container.clone().upcast(),
        }
    }

    /// Applies a size mid-drag. A `Terminal` additionally keeps the VTE
    /// character grid in step (see `SessionNode::request_grid`); a `Note`
    /// additionally schedules a Preview re-render (see
    /// `NoteNode::refresh_preview_after_resize`'s doc comment for why a
    /// resize needs that); every other kind just requests a widget size.
    fn apply_resize(&self, size: (f64, f64)) {
        match self {
            NodeWidget::Terminal(node) => node.request_grid(size.0, size.1),
            NodeWidget::Note(node) => {
                self.container()
                    .set_size_request(size.0 as i32, size.1 as i32);
                node.refresh_preview_after_resize();
            }
            NodeWidget::Text(_) | NodeWidget::Placeholder(_) => {
                self.container()
                    .set_size_request(size.0 as i32, size.1 as i32);
            }
        }
    }

    /// Collapses/expands the node to just its title bar. The only way this
    /// state ever changed used to be a direct click on the node's own
    /// minimize button (`node::wire_minimize`'s closure) — `App`'s
    /// "Collapse Selected"/"Expand Selected" menu commands, and undo/redo of
    /// a collapse toggle, updated `NodeRecord::collapsed` without this call
    /// and so silently left the widget showing the opposite of what was
    /// persisted.
    fn set_collapsed(&self, collapsed: bool) {
        match self {
            NodeWidget::Terminal(node) => node.collapse.set_collapsed(collapsed),
            NodeWidget::Note(node) => node.collapse.set_collapsed(collapsed),
            NodeWidget::Text(node) => node.collapse.set_collapsed(collapsed),
            NodeWidget::Placeholder(node) => node.collapse.set_collapsed(collapsed),
        }
    }
}

pub struct NodeEntry {
    pub record: NodeRecord,
    pub widget: NodeWidget,
    /// The character grid last pushed to a `Terminal` node's PTY, so
    /// `pump_output`'s sync only sends a `SIGWINCH` when the grid actually
    /// changed. Meaningless (left `None`) for every other kind.
    pub pty_grid: Option<(u16, u16)>,
    /// Whether the "exited" status badge has already been shown. Meaningless
    /// for every non-`Terminal` kind.
    pub exit_shown: bool,
}

/// One undoable canvas edit. Each variant stores exactly what its own
/// inverse needs — the "before" state for an undo, which doubles as the
/// "after" state for the matching redo, so `App::undo`/`App::redo` share one
/// `apply` function per variant rather than duplicating forward/backward
/// logic.
pub enum CanvasCommand {
    /// Covers node creation (undo removes them), multi-select delete/
    /// duplicate/paste (undo re-adds exactly what was removed/created), and
    /// restore of removed edges with their endpoints. A single node is just
    /// a one-element `Vec` — "one coherent edit" is the unit, not "one node".
    AddNodes {
        nodes: Vec<NodeRecord>,
        edges: Vec<EdgeRecord>,
    },
    RemoveNodes {
        nodes: Vec<NodeRecord>,
        edges: Vec<EdgeRecord>,
    },
    /// A drag (of one or many selected nodes together) coalesced into one
    /// entry per node, captured once at drag-end — never per pointer-motion
    /// tick, which is what keeps a whole drag a single undo step.
    MoveNodes {
        moves: Vec<NodeMove>,
    },
    ResizeNode {
        id: Uuid,
        old_size: (f64, f64),
        new_size: (f64, f64),
    },
    AddEdge {
        edge: EdgeRecord,
    },
    RemoveEdge {
        edge: EdgeRecord,
    },
    /// A batch property change (collapse/lock toggles on a selection,
    /// typically) as paired before/after snapshots of the affected records.
    SetProperties {
        before: Vec<NodeRecord>,
        after: Vec<NodeRecord>,
    },
}

/// Whether a workspace's widgets are on screen, its processes are merely
/// running in the background, or nothing about it is live at all. Computed
/// on demand (`App::workspace_runtime_state`), never persisted — this is
/// exactly the kind of fact that's only true of the current run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRuntimeState {
    Active,
    Background,
    Unloaded,
}

pub struct App {
    pub accounts: AccountStore,
    pub store_path: PathBuf,
    pub canvas: Canvas,
    /// The *active* workspace's identity. Its nodes/edges/canvas are the live
    /// state below — only one workspace is ever loaded into real GTK widgets
    /// and PTYs at a time.
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub workspace_root: PathBuf,
    /// The rest of the active workspace's metadata, mirrored from its
    /// `WorkspaceRecord` the same way `workspace_id`/`name`/`root` already
    /// are — see `activate_workspace`/`snapshot_active_workspace`.
    pub workspace_environment: EnvironmentKind,
    pub workspace_color: Option<String>,
    pub workspace_icon: Option<String>,
    pub workspace_created_at: u64,
    pub workspace_last_opened: u64,
    /// Every OTHER workspace's full persisted state: not live (no widgets, no
    /// PTYs), just the records needed to spawn it back in if the user
    /// switches to it. `switch_workspace`/`create_workspace`/
    /// `delete_workspace` are the only places that move a workspace between
    /// "this" (live) and an entry here (dormant).
    pub inactive_workspaces: Vec<WorkspaceRecord>,
    /// User-created roles, global across every workspace. Built-in roles
    /// come from `role::builtin_roles` and are never stored here.
    pub custom_roles: Vec<Role>,
    pub nodes: HashMap<Uuid, NodeEntry>,
    /// Every live `Terminal` node's PTY/process, keyed by the same id as
    /// `nodes`. See `runtime.rs`'s module doc for why this is a separate map.
    pub runtime: SessionRuntime,
    pub edges: Vec<EdgeRecord>,
    /// A capped, in-memory log of agent-to-agent messages sent via
    /// `control.rs` — not persisted, just enough to answer "what was just
    /// sent".
    pub messages: Vec<AgentMessage>,
    /// Set while the user has clicked a node's link button and is waiting to
    /// click a target node to complete the edge. `None` otherwise.
    pub pending_edge_source: Option<Uuid>,
    /// The edge id the user clicked on the canvas, highlighted and armed so
    /// a second click on it deletes it. `None` when nothing is selected.
    pub selected_edge: Option<Uuid>,
    /// Every currently-selected node, by id. Single click replaces this
    /// wholesale with one id; Ctrl/Shift-click toggles membership; marquee
    /// replaces it with everything inside the dragged rectangle.
    pub selected: HashSet<Uuid>,
    /// In-canvas clipboard for copy/paste — plain `NodeRecord` snapshots, not
    /// the system clipboard (cross-application paste of a live session makes
    /// no sense; a fresh process is spawned on paste the same as duplicate).
    pub clipboard: Vec<NodeRecord>,
    pub undo_stack: Vec<CanvasCommand>,
    pub redo_stack: Vec<CanvasCommand>,
    /// Whether a node's position snaps to `canvas::GRID_MINOR` at drag-end.
    pub snap_to_grid: bool,
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
            workspace_environment: EnvironmentKind::default(),
            workspace_color: None,
            workspace_icon: None,
            workspace_created_at: now_epoch_secs(),
            workspace_last_opened: now_epoch_secs(),
            inactive_workspaces: Vec::new(),
            custom_roles: Vec::new(),
            nodes: HashMap::new(),
            runtime: SessionRuntime::new(),
            edges: Vec::new(),
            messages: Vec::new(),
            pending_edge_source: None,
            selected_edge: None,
            selected: HashSet::new(),
            clipboard: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            snap_to_grid: false,
            pending_save: None,
        }))
    }

    /// Debounces `persist()` so a burst of rapid-fire events (e.g. every
    /// keystroke in a note's text buffer) collapses into a single disk write
    /// `PERSIST_DEBOUNCE` after the last one, instead of each event doing its
    /// own synchronous fsync+rename. Callers must still update any in-memory
    /// state synchronously before calling this — only the disk write is
    /// delayed.
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
    /// workspace, if that id isn't found) the live one, and spawns its nodes
    /// into the canvas. Every other workspace's records are kept dormant in
    /// `inactive_workspaces` until switched to. A brand-new install (no store
    /// file yet) keeps the placeholder workspace `App::new` already set up.
    /// `toast_overlay` is threaded down into `spawn_workspace_contents` so a
    /// later failed handoff on a restored session can surface its own toast.
    pub fn restore(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) -> Vec<String> {
        let (saved, load_warning) = {
            let app_ref = app.borrow();
            Store::load_with_warning(&app_ref.store_path)
        };
        let mut errors: Vec<String> = load_warning.into_iter().collect();
        app.borrow_mut().custom_roles = saved.custom_roles.clone();

        let Some((active, inactive)) = saved.resolve_active_workspace() else {
            return errors;
        };

        app.borrow_mut().inactive_workspaces = inactive;
        App::activate_workspace(app, &active);

        errors.extend(spawn_workspace_contents(
            app,
            active.nodes,
            active.edges,
            toast_overlay,
        ));
        errors
    }

    pub fn persist(&self) -> std::io::Result<()> {
        let mut workspaces = self.inactive_workspaces.clone();
        workspaces.push(self.snapshot_active_workspace());
        let store = Store::new(
            workspaces,
            Some(self.workspace_id),
            self.custom_roles.clone(),
        );
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

    /// The instructions text for a session's assigned role, if it has one.
    /// `None` both when no role is assigned and when the assigned role no
    /// longer exists.
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
    /// on every live terminal currently assigned to it.
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
            for entry in app_mut.nodes.values_mut() {
                let NodeWidget::Terminal(node) = &entry.widget else {
                    continue;
                };
                if entry.record.as_terminal().and_then(|t| t.role_id) == Some(id) {
                    node.set_role(Some(&name), icon.as_deref(), accent.as_deref());
                }
            }
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// Deletes a custom role, unassigning it from every terminal that
    /// referenced it rather than closing anything.
    pub fn delete_role(app: &Rc<RefCell<App>>, id: Uuid) -> anyhow::Result<()> {
        {
            let mut app_mut = app.borrow_mut();
            app_mut.custom_roles.retain(|role| role.id != id);
            for entry in app_mut.nodes.values_mut() {
                let NodeWidget::Terminal(node) = &entry.widget else {
                    continue;
                };
                if let Some(terminal) = entry.record.as_terminal_mut()
                    && terminal.role_id == Some(id)
                {
                    terminal.role_id = None;
                    node.set_role(None, None, None);
                }
            }
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// The active workspace's current live state, as a `WorkspaceRecord`.
    fn snapshot_active_workspace(&self) -> WorkspaceRecord {
        let state = self.canvas.state.borrow();
        WorkspaceRecord {
            id: self.workspace_id,
            name: self.workspace_name.clone(),
            root_dir: self.workspace_root.clone(),
            nodes: self
                .nodes
                .values()
                .map(|entry| entry.record.clone())
                .collect(),
            edges: self.edges.clone(),
            canvas: CanvasRecord {
                zoom: state.zoom,
                pan: state.pan,
            },
            environment: self.workspace_environment,
            color: self.workspace_color.clone(),
            icon: self.workspace_icon.clone(),
            created_at: self.workspace_created_at,
            last_opened: self.workspace_last_opened,
        }
    }

    /// Lists every workspace (the active one included) as `(id, name)`,
    /// sorted by name.
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

    /// Removes the active workspace's GTK widgets from the canvas and clears
    /// its live node/edge/selection/undo state, WITHOUT touching the
    /// runtime — the outgoing workspace's terminal processes keep running in
    /// the background. This is "switching away", not "destroying": callers
    /// are responsible for snapshotting the workspace first if it should
    /// come back later (every caller does, via `stash_workspace`).
    /// `switch_workspace` and `create_workspace` both use this, since
    /// starting to view a different workspace must never stop another one's
    /// agents from working. Compare `teardown_active_workspace`, which
    /// additionally terminates every session — reserved for genuinely
    /// destroying the active workspace (`delete_workspace`).
    fn detach_active_workspace(app: &Rc<RefCell<App>>) {
        let (canvas, node_entries) = {
            let mut app_mut = app.borrow_mut();
            let node_entries: Vec<_> = app_mut.nodes.drain().collect();
            (app_mut.canvas.clone(), node_entries)
        };
        for (_, entry) in node_entries {
            canvas.remove_node(entry.widget.container());
        }
        {
            let mut app_mut = app.borrow_mut();
            app_mut.edges.clear();
            app_mut.pending_edge_source = None;
            app_mut.selected_edge = None;
            app_mut.selected.clear();
            app_mut.undo_stack.clear();
            app_mut.redo_stack.clear();
        }
    }

    /// Kills every live `Terminal`'s process (and, for a `LocalTmux`
    /// terminal, its underlying tmux session too — see
    /// `environment::kill_tmux_session`; a plain `terminate` only ends
    /// `duet`'s own client process, which for tmux would just detach,
    /// leaving the real work running) and then detaches the active
    /// workspace's widgets same as `detach_active_workspace`. Reserved for
    /// genuinely destroying the active workspace (`delete_workspace`) — see
    /// `detach_active_workspace` for the far more common "just switching
    /// away" case.
    fn teardown_active_workspace(app: &Rc<RefCell<App>>) {
        let terminals: Vec<(Uuid, EnvironmentKind)> = app
            .borrow()
            .nodes
            .iter()
            .filter_map(|(id, entry)| entry.record.as_terminal().map(|t| (*id, t.environment)))
            .collect();
        for (id, env) in terminals {
            environment::terminate(&mut app.borrow_mut().runtime, id, env);
        }
        App::detach_active_workspace(app);
    }

    /// Replaces or inserts `record` into `inactive_workspaces` by id.
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
    /// and its live `Canvas` state — but does NOT spawn its nodes; that's
    /// `spawn_workspace_contents`'s job, called separately right after this.
    fn activate_workspace(app: &Rc<RefCell<App>>, record: &WorkspaceRecord) {
        let mut app_mut = app.borrow_mut();
        app_mut.workspace_id = record.id;
        app_mut.workspace_name = record.name.clone();
        app_mut.workspace_root = record.root_dir.clone();
        app_mut.workspace_environment = record.environment;
        app_mut.workspace_color = record.color.clone();
        app_mut.workspace_icon = record.icon.clone();
        app_mut.workspace_created_at = record.created_at;
        // Bumped to now, not copied from `record.last_opened` — this call is
        // itself the moment the workspace is being opened.
        app_mut.workspace_last_opened = now_epoch_secs();
        let mut state = app_mut.canvas.state.borrow_mut();
        state.zoom = record.canvas.zoom;
        state.pan = record.canvas.pan;
    }

    /// Makes `target_id` the active workspace. A no-op if already active.
    /// Persists on success.
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
        App::detach_active_workspace(app);
        App::stash_workspace(app, outgoing);

        App::activate_workspace(app, &target);
        let errors = spawn_workspace_contents(app, target.nodes, target.edges, toast_overlay);
        let _ = app.borrow().persist();
        errors
    }

    /// Creates a brand-new, empty workspace and switches to it immediately.
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
        App::detach_active_workspace(app);
        App::stash_workspace(app, outgoing);

        let new_id = Uuid::new_v4();
        {
            let mut app_mut = app.borrow_mut();
            app_mut.workspace_id = new_id;
            app_mut.workspace_name = name;
            app_mut.workspace_root = root_dir;
            app_mut.workspace_environment = EnvironmentKind::default();
            app_mut.workspace_color = None;
            app_mut.workspace_icon = None;
            app_mut.workspace_created_at = now_epoch_secs();
            app_mut.workspace_last_opened = now_epoch_secs();
        }
        {
            let app_ref = app.borrow();
            *app_ref.canvas.state.borrow_mut() = crate::canvas::CanvasState::new();
        }
        app.borrow().persist()?;
        Ok(new_id)
    }

    /// Renames a workspace, active or dormant.
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

    /// Deletes a workspace — tearing down its sessions first if it's the
    /// active one — after confirming at least one other workspace exists to
    /// fall back to.
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
            spawn_workspace_contents(app, next.nodes, next.edges, toast_overlay);
        } else {
            // A dormant (Background or Unloaded) workspace can still own
            // live runtime sessions since Milestone 2 — deleting its record
            // without terminating those first would leak them: nothing
            // would reference that id again. `unload_workspace`'s own
            // termination loop does exactly this; reuse it rather than
            // duplicating it here.
            let _ = App::unload_workspace(app, id);
            app.borrow_mut().inactive_workspaces.retain(|w| w.id != id);
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// Kills every live process belonging to a *dormant* (non-active)
    /// workspace, without touching its persisted record — the workspace
    /// moves from Background (processes alive, not shown) to Unloaded (no
    /// processes). Switching back to it afterwards spawns fresh processes,
    /// exactly like opening it for the first time. Refuses to operate on the
    /// active workspace, which has no sense of "unload" distinct from
    /// `teardown_active_workspace`/switching away — there's no dormant
    /// record to look up its nodes from until it's been switched out of.
    /// Cross-workspace isolation falls out of this only ever touching the
    /// named workspace's own node ids, which (being globally unique UUIDs)
    /// can never collide with another workspace's.
    pub fn unload_workspace(app: &Rc<RefCell<App>>, id: Uuid) -> anyhow::Result<()> {
        if app.borrow().workspace_id == id {
            anyhow::bail!("switch away from a workspace before unloading it");
        }
        let terminals: Vec<(Uuid, EnvironmentKind)> = {
            let app_ref = app.borrow();
            let workspace = app_ref
                .inactive_workspaces
                .iter()
                .find(|w| w.id == id)
                .context("workspace not found")?;
            workspace
                .nodes
                .iter()
                .filter_map(|node| node.as_terminal().map(|t| (node.id, t.environment)))
                .collect()
        };
        for (node_id, env) in terminals {
            environment::terminate(&mut app.borrow_mut().runtime, node_id, env);
        }
        Ok(())
    }

    /// Whether `id` is the visible workspace, a dormant one with live
    /// background processes, or a dormant one with none. Purely a runtime
    /// read — not persisted, and not knowable at all for the active
    /// workspace's own terminals beyond "Active" (its nodes are always live
    /// while it's active, by construction).
    pub fn workspace_runtime_state(&self, id: Uuid) -> WorkspaceRuntimeState {
        if id == self.workspace_id {
            return WorkspaceRuntimeState::Active;
        }
        let Some(workspace) = self.inactive_workspaces.iter().find(|w| w.id == id) else {
            return WorkspaceRuntimeState::Unloaded;
        };
        let any_alive = workspace
            .nodes
            .iter()
            .any(|node| self.runtime.is_alive(node.id));
        if any_alive {
            WorkspaceRuntimeState::Background
        } else {
            WorkspaceRuntimeState::Unloaded
        }
    }

    /// Drains every `Terminal` node's PTY output into its own widget. Called
    /// on a timer from `main.rs`.
    pub fn pump_output(&mut self) {
        // Every session this registry tracks, not just the active
        // workspace's visible ones — a backgrounded workspace's terminals
        // keep producing output on an unbounded channel (`runtime.rs`'s
        // `try_recv_output`), so they still need draining or that channel
        // grows without bound while nobody's watching. The drained bytes are
        // discarded, not buffered for replay: reattaching a background
        // workspace shows only output from that point on, not historical
        // scrollback — a deliberate, documented gap (see `runtime.rs`'s
        // module doc), not full crash-recovery machinery.
        for id in self.runtime.live_ids() {
            if !self.nodes.contains_key(&id) {
                let _ = self.runtime.try_recv_output(id);
            }
        }
        for (&id, entry) in self.nodes.iter_mut() {
            let NodeWidget::Terminal(node) = &entry.widget else {
                continue;
            };
            // The one authoritative place the PTY is sized, from VTE's real
            // post-allocation grid. See `SessionNode::request_grid` for why
            // a request is not what VTE ends up rendering.
            if let Some(grid) = node.actual_grid()
                && entry.pty_grid != Some(grid)
            {
                let _ = self.runtime.resize(id, grid.1, grid.0);
                entry.pty_grid = Some(grid);
            }
            let chunks = self.runtime.try_recv_output(id);
            for chunk in &chunks {
                node.feed(chunk);
            }
            if !entry.exit_shown && self.runtime.has_exited(id) {
                // `activity` only ever distinguishes Finished/Failed from a
                // real exit status this milestone (see `AgentActivity`'s doc
                // comment) — safe to surface directly as the badge text.
                let label = if self.runtime.activity(id) == AgentActivity::Failed {
                    "failed"
                } else {
                    "exited"
                };
                node.status_label.set_text(label);
                entry.exit_shown = true;
            }
        }
    }

    /// Every live `Terminal`, as `duet agent list` (via `control.rs`)
    /// reports it.
    pub fn agent_summaries(&self) -> Vec<AgentSummary> {
        self.nodes
            .values()
            .filter_map(|entry| {
                let terminal = entry.record.as_terminal()?;
                Some(AgentSummary {
                    id: entry.record.id,
                    name: terminal.name.clone(),
                    agent: terminal.agent.display_name(),
                })
            })
            .collect()
    }

    /// Every edge between two `Terminal` nodes, by name, for `duet agent
    /// list`'s output. Silently drops an edge whose endpoint isn't a live
    /// terminal.
    pub fn link_summaries(&self) -> Vec<LinkSummary> {
        self.edges
            .iter()
            .filter_map(|edge| {
                let source = self
                    .nodes
                    .get(&edge.source)?
                    .record
                    .as_terminal()?
                    .name
                    .clone();
                let target = self
                    .nodes
                    .get(&edge.target)?
                    .record
                    .as_terminal()?
                    .name
                    .clone();
                Some(LinkSummary { source, target })
            })
            .collect()
    }

    /// Resolves `duet agent send`'s target: an exact node id, or failing
    /// that an exact (case-sensitive) terminal name.
    fn find_session_id(&self, target: &str) -> Option<Uuid> {
        if let Ok(id) = Uuid::parse_str(target)
            && self
                .nodes
                .get(&id)
                .is_some_and(|e| e.record.as_terminal().is_some())
        {
            return Some(id);
        }
        self.nodes
            .iter()
            .find(|(_, entry)| entry.record.as_terminal().is_some_and(|t| t.name == target))
            .map(|(id, _)| *id)
    }

    /// Delivers a structured agent-to-agent message: writes a clearly
    /// labeled envelope into the target terminal's PTY input, and records
    /// the attempt in `messages`.
    pub fn send_message(
        app: &Rc<RefCell<App>>,
        source_session_id: Option<Uuid>,
        target: &str,
        content: String,
    ) -> Result<AgentMessage, String> {
        let mut app_mut = app.borrow_mut();
        let Some(target_id) = app_mut.find_session_id(target) else {
            return Err(format!("no agent named '{target}'"));
        };
        let source_session_id = source_session_id.filter(|id| app_mut.nodes.contains_key(id));
        let source_label = source_session_id
            .and_then(|id| app_mut.nodes.get(&id))
            .and_then(|entry| entry.record.as_terminal().map(|t| (t, entry)))
            .map(|(terminal, _)| format!("{} ({})", terminal.name, terminal.agent.display_name()))
            .unwrap_or_else(|| "an external sender".to_string());
        let envelope = crate::message::message_envelope(&source_label, &content);
        let delivered = app_mut.runtime.write_input(target_id, envelope.as_bytes());
        let status = if delivered {
            DeliveryStatus::Delivered
        } else {
            DeliveryStatus::Failed
        };
        if delivered {
            let app = Rc::clone(app);
            glib::timeout_add_local_once(MESSAGE_SUBMIT_DELAY, move || {
                let _ = app.borrow_mut().runtime.write_input(target_id, b"\r");
            });
        }
        let message = AgentMessage {
            id: Uuid::new_v4(),
            source: source_session_id,
            target: target_id,
            content,
            timestamp: crate::message::now_epoch_secs(),
            status,
        };
        app_mut.messages.push(message.clone());
        if app_mut.messages.len() > MESSAGE_LOG_LIMIT {
            let overflow = app_mut.messages.len() - MESSAGE_LOG_LIMIT;
            app_mut.messages.drain(0..overflow);
        }
        Ok(message)
    }

    /// Spawns a brand-new terminal node at `viewport_center_world`, wires it,
    /// and persists. Used by the new-session dialog in `main.rs`.
    // Every parameter is an independent piece of what the new-session dialog
    // collected; bundling them into a params struct would just move the same
    // fields one level out without clarifying anything at this single call site.
    #[allow(clippy::too_many_arguments)]
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
                .nodes
                .values()
                .any(|entry| entry.record.as_terminal().is_some_and(|t| t.name == name))
            {
                anyhow::bail!("a session named '{name}' already exists");
            }
        }

        let record = {
            let app_ref = app.borrow();
            build_terminal_record(
                &app_ref,
                &name,
                &cwd,
                agent,
                claude_account,
                role_id,
                viewport_center_world,
            )?
        };
        materialize_node(app, record.clone(), toast_overlay)
            .map_err(|error| anyhow::anyhow!(error))?;
        App::push_undo(
            app,
            CanvasCommand::AddNodes {
                nodes: vec![record],
                edges: Vec::new(),
            },
        );
        app.borrow().persist()?;
        Ok(())
    }

    /// Spawns a blank Markdown note at `position`, wires it, and persists.
    pub fn create_note(app: &Rc<RefCell<App>>, position: (f64, f64)) {
        let record = NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position,
            size: (220.0, 160.0),
            z_order: next_z_order(&app.borrow()),
            collapsed: false,
            locked: false,
            kind: NodeKind::Note(NotePayload {
                markdown: String::new(),
                color: "yellow".to_string(),
                view_mode: NoteViewMode::Edit,
            }),
        };
        let _ = materialize_node(app, record.clone(), &adw::ToastOverlay::new());
        App::push_undo(
            app,
            CanvasCommand::AddNodes {
                nodes: vec![record],
                edges: Vec::new(),
            },
        );
        let _ = app.borrow().persist();
    }

    /// Spawns a blank plain-text node at `position`.
    pub fn create_text_node(app: &Rc<RefCell<App>>, position: (f64, f64)) {
        let record = NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position,
            size: (220.0, 160.0),
            z_order: next_z_order(&app.borrow()),
            collapsed: false,
            locked: false,
            kind: NodeKind::Text(TextPayload {
                content: String::new(),
            }),
        };
        let _ = materialize_node(app, record.clone(), &adw::ToastOverlay::new());
        App::push_undo(
            app,
            CanvasCommand::AddNodes {
                nodes: vec![record],
                edges: Vec::new(),
            },
        );
        let _ = app.borrow().persist();
    }

    /// Spawns a placeholder node of the given kind (`FileTree`/`Portal`/
    /// `Drawing`/`Group`) at `position`. `kind` builds its own default,
    /// empty payload — see `model.rs`'s placeholder payload types.
    pub fn create_placeholder_node(app: &Rc<RefCell<App>>, kind: NodeKind, position: (f64, f64)) {
        let record = NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position,
            size: (220.0, 160.0),
            z_order: next_z_order(&app.borrow()),
            collapsed: false,
            locked: false,
            kind,
        };
        let _ = materialize_node(app, record.clone(), &adw::ToastOverlay::new());
        App::push_undo(
            app,
            CanvasCommand::AddNodes {
                nodes: vec![record],
                edges: Vec::new(),
            },
        );
        let _ = app.borrow().persist();
    }

    /// Records a logical connection between `source` and `target`. A no-op
    /// if the edge already exists, `source == target`, or either id no
    /// longer names a live node. Returns whether a new edge was recorded.
    pub fn create_edge(app: &Rc<RefCell<App>>, source: Uuid, target: Uuid) -> bool {
        let mut app = app.borrow_mut();
        if source != target
            && app.nodes.contains_key(&source)
            && app.nodes.contains_key(&target)
            && !app
                .edges
                .iter()
                .any(|e| e.source == source && e.target == target)
        {
            let edge = EdgeRecord::visual(Uuid::new_v4(), source, target);
            app.edges.push(edge.clone());
            app.undo_stack.push(CanvasCommand::AddEdge { edge });
            app.redo_stack.clear();
            let _ = app.persist();
            return true;
        }
        false
    }

    /// Puts the `link-source` CSS class on exactly the node named by
    /// `pending_edge_source`, and on no other.
    fn refresh_link_highlight(&self) {
        for (&id, entry) in &self.nodes {
            if self.pending_edge_source == Some(id) {
                entry.widget.container().add_css_class("link-source");
            } else {
                entry.widget.container().remove_css_class("link-source");
            }
        }
    }

    /// Every live edge's world-space endpoints, chosen from the sides that
    /// face each other so stacked cards use a vertical route.
    pub fn edge_lines(&self) -> Vec<EdgeEndpoints> {
        self.edges
            .iter()
            .filter_map(|edge| {
                let source = node_geometry(self.nodes.get(&edge.source)?);
                let target = node_geometry(self.nodes.get(&edge.target)?);
                let source_center = card_center(source);
                let target_center = card_center(target);
                let horizontal = (target_center.0 - source_center.0).abs()
                    >= (target_center.1 - source_center.1).abs();
                let (from, to) = if horizontal {
                    let source_is_left = source_center.0 <= target_center.0;
                    (
                        card_edge(source, source_is_left),
                        card_edge(target, !source_is_left),
                    )
                } else {
                    let source_is_above = source_center.1 <= target_center.1;
                    (
                        card_vertical_edge(source, source_is_above),
                        card_vertical_edge(target, !source_is_above),
                    )
                };
                Some((edge.clone(), from, to))
            })
            .collect()
    }

    pub fn edge_overlap(&self, edge_id: Uuid) -> Option<[(f64, f64, f64, f64); 2]> {
        let edge = self.edges.iter().find(|e| e.id == edge_id)?;
        let source = node_geometry(self.nodes.get(&edge.source)?);
        let target = node_geometry(self.nodes.get(&edge.target)?);
        card_intersection(source, target).map(|_| [card_rect(source), card_rect(target)])
    }

    /// Whether any node covers `world` (world-space). Used to keep a click
    /// that landed on a node from also being treated as a click on an edge
    /// line, since the lines are painted behind the nodes — and, in
    /// `main.rs`'s background-click handler, to tell "truly empty canvas"
    /// apart from "landed on a card" before clearing the selection (both
    /// cases make `click_link_at` return `None`, since neither is a link
    /// hit, but only the first should deselect — the second is a button
    /// click, a drag-to-move, or a click into a note's text view, none of
    /// which should discard whatever was selected).
    pub fn covers_point(&self, world: (f64, f64)) -> bool {
        self.nodes.values().any(|entry| {
            let (position, size) = (entry.record.position, entry.record.size);
            world.0 >= position.0
                && world.0 <= position.0 + size.0
                && world.1 >= position.1
                && world.1 <= position.1 + size.1 + TITLE_BAR_HEIGHT
        })
    }

    /// Click-to-select, click-again-to-delete for edge lines. A click that
    /// misses every edge just clears the selection.
    pub fn click_link_at(app: &Rc<RefCell<App>>, world: (f64, f64)) -> Option<String> {
        let (hit, previous) = {
            let app_ref = app.borrow();
            let tolerance = 12.0 / app_ref.canvas.state.borrow().zoom;
            let hit = if app_ref.covers_point(world) {
                None
            } else {
                app_ref
                    .edge_lines()
                    .into_iter()
                    .map(|(edge, from, to)| {
                        (edge, crate::canvas::distance_to_link(from, to, world))
                    })
                    .filter(|(_, distance)| *distance <= tolerance)
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(edge, _)| edge)
            };
            (hit, app_ref.selected_edge)
        };
        match hit {
            None => {
                app.borrow_mut().selected_edge = None;
                None
            }
            Some(edge) if previous == Some(edge.id) => {
                App::remove_edge(app, edge.id);
                app.borrow_mut().selected_edge = None;
                let app_ref = app.borrow();
                let name = |id: Uuid| node_display_name(&app_ref, id);
                Some(format!(
                    "unlinked {} -> {}",
                    name(edge.source),
                    name(edge.target)
                ))
            }
            Some(edge) => {
                app.borrow_mut().selected_edge = Some(edge.id);
                Some("link selected — click it again to delete".to_string())
            }
        }
    }

    pub fn remove_edge(app: &Rc<RefCell<App>>, edge_id: Uuid) {
        let mut app = app.borrow_mut();
        let Some(index) = app.edges.iter().position(|e| e.id == edge_id) else {
            return;
        };
        let edge = app.edges.remove(index);
        app.undo_stack.push(CanvasCommand::RemoveEdge { edge });
        app.redo_stack.clear();
        let _ = app.persist();
    }

    /// Enters "link mode" for `source`: the next node clicked becomes the
    /// edge's target.
    pub fn start_link(app: &Rc<RefCell<App>>, source: Uuid) {
        app.borrow_mut().pending_edge_source = Some(source);
        app.borrow().refresh_link_highlight();
    }

    /// If an edge is pending (from `start_link`), completes it with `target`
    /// and clears the pending state.
    pub fn complete_link_if_pending(app: &Rc<RefCell<App>>, target: Uuid) -> Option<String> {
        let source = app.borrow_mut().pending_edge_source.take()?;
        let created = App::create_edge(app, source, target);
        let app_ref = app.borrow();
        app_ref.refresh_link_highlight();
        if !created {
            return None;
        }
        let name = |id: Uuid| node_display_name(&app_ref, id);
        Some(format!("linked {} -> {}", name(source), name(target)))
    }

    /// Commits an inline title rename (see `wire_rename`).
    pub fn rename_session(app: &Rc<RefCell<App>>, id: Uuid, name: &str) -> anyhow::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("give this session a name");
        }
        {
            let app_ref = app.borrow();
            if app_ref.nodes.iter().any(|(&other, entry)| {
                other != id && entry.record.as_terminal().is_some_and(|t| t.name == name)
            }) {
                anyhow::bail!("a session named '{name}' already exists");
            }
        }
        {
            let mut app_mut = app.borrow_mut();
            let entry = app_mut.nodes.get_mut(&id).context("session not found")?;
            let NodeWidget::Terminal(node) = &entry.widget else {
                anyhow::bail!("not a terminal node");
            };
            node.set_name(name);
            entry
                .record
                .as_terminal_mut()
                .context("not a terminal node")?
                .name = name.to_string();
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// Hands a session off from its current agent to the other one.
    pub fn switch_agent(app: &Rc<RefCell<App>>, id: Uuid) -> anyhow::Result<()> {
        let record = {
            let app_ref = app.borrow();
            app_ref
                .nodes
                .get(&id)
                .context("session not found")?
                .record
                .as_terminal()
                .context("not a terminal node")?
                .clone()
        };
        if !record.agent.supports_handoff() {
            anyhow::bail!(
                "{} sessions don't support handing off to another agent",
                record.agent.display_name()
            );
        }
        environment::terminate(&mut app.borrow_mut().runtime, id, record.environment);

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
        let (launch, updated) = {
            let app_ref = app.borrow();
            let initial_prompt =
                with_role_instructions(app_ref.role_instructions(record.role_id), Some(&summary));
            if matches!(to, Agent::Claude) {
                let account = crate::account::DEFAULT_ACCOUNT.to_string();
                let config_dir = app_ref.accounts.ensure(&account)?;
                let session_id = Uuid::new_v4();
                let launch = with_session_env(
                    to.launch(LaunchRequest {
                        initial_prompt: initial_prompt.as_deref(),
                        claude_session_id: Some(session_id),
                        claude_config_dir: Some(&config_dir),
                        ..Default::default()
                    }),
                    id,
                );
                let mut updated = record.clone();
                updated.agent = Agent::Claude;
                updated.claude_session_id = Some(session_id);
                updated.claude_account = Some(account);
                (launch, updated)
            } else {
                let launch = with_session_env(
                    to.launch(LaunchRequest {
                        initial_prompt: initial_prompt.as_deref(),
                        ..Default::default()
                    }),
                    id,
                );
                let mut updated = record.clone();
                updated.agent = Agent::Codex;
                updated.claude_session_id = None;
                updated.claude_account = None;
                (launch, updated)
            }
        };
        let prepared = environment::prepare_launch(updated.environment, id, launch);
        app.borrow_mut()
            .runtime
            .spawn(id, updated.cwd.clone(), prepared)?;
        if let Some(entry) = app.borrow_mut().nodes.get_mut(&id)
            && let Some(terminal) = entry.record.as_terminal_mut()
        {
            *terminal = updated;
        }
        app.borrow().persist()?;
        Ok(())
    }

    /// Removes every terminal using account `name`, then the account's
    /// on-disk config directory.
    pub fn delete_account(app: &Rc<RefCell<App>>, name: &str) -> anyhow::Result<()> {
        let to_remove: Vec<Uuid> = {
            let app_ref = app.borrow();
            app_ref
                .nodes
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .record
                        .as_terminal()
                        .is_some_and(|t| t.claude_account.as_deref() == Some(name))
                })
                .map(|(id, _)| *id)
                .collect()
        };
        for id in &to_remove {
            App::close_node(app, *id);
        }
        app.borrow().accounts.remove(name)?;
        app.borrow().persist()?;
        Ok(())
    }

    /// Removes a single node from the canvas: kills its process if it's a
    /// terminal, drops its edges, clears any dangling selection/pending-link
    /// state, and persists. Does not push an undo entry on its own — see
    /// `delete_selected` for the undoable multi-node path; this is also used
    /// internally (e.g. `delete_account`) where undo doesn't apply.
    pub fn close_node(app: &Rc<RefCell<App>>, id: Uuid) {
        let (canvas, removed) = {
            let mut app_mut = app.borrow_mut();
            let canvas = app_mut.canvas.clone();
            let removed = app_mut.nodes.remove(&id);
            let env = removed
                .as_ref()
                .and_then(|entry| entry.record.as_terminal())
                .map(|t| t.environment)
                .unwrap_or_default();
            environment::terminate(&mut app_mut.runtime, id, env);
            app_mut.edges.retain(|e| e.source != id && e.target != id);
            if app_mut.pending_edge_source == Some(id) {
                app_mut.pending_edge_source = None;
            }
            if app_mut.selected_edge.is_some_and(|edge_id| {
                app_mut
                    .edges
                    .iter()
                    .find(|e| e.id == edge_id)
                    .is_none_or(|e| e.source == id || e.target == id)
            }) {
                app_mut.selected_edge = None;
            }
            app_mut.selected.remove(&id);
            app_mut.refresh_link_highlight();
            (canvas, removed)
        };
        if let Some(entry) = removed {
            canvas.remove_node(entry.widget.container());
        }
        let _ = app.borrow().persist();
    }

    /// Kills the process for every selected `Terminal` node without removing
    /// the node itself — the card stays on the canvas, now showing "exited"
    /// (the same badge a process that dies on its own gets via
    /// `pump_output`), restartable later via `restart_selected_terminals`.
    /// Distinct from `delete_selected`/`close_node`, which removes the card
    /// too — "terminate" and "delete" are different operations.
    pub fn terminate_selected_terminals(app: &Rc<RefCell<App>>) {
        let targets: Vec<(Uuid, EnvironmentKind)> = {
            let app_ref = app.borrow();
            app_ref
                .selected
                .iter()
                .filter_map(|id| {
                    app_ref
                        .nodes
                        .get(id)
                        .and_then(|entry| entry.record.as_terminal())
                        .map(|terminal| (*id, terminal.environment))
                })
                .collect()
        };
        for (id, env) in targets {
            environment::terminate(&mut app.borrow_mut().runtime, id, env);
            let mut app_mut = app.borrow_mut();
            if let Some(entry) = app_mut.nodes.get_mut(&id) {
                if let NodeWidget::Terminal(node) = &entry.widget {
                    node.status_label.set_text("exited");
                }
                entry.exit_shown = true;
            }
        }
    }

    /// Respawns a fresh process for every selected `Terminal` node,
    /// replacing whatever it had (running or already exited) — the
    /// complement of `terminate_selected_terminals`. For a `LocalTmux`
    /// terminal this is a genuine restart, not a reconnect: the tmux session
    /// itself is killed (via `environment::terminate`) before a new one is
    /// created under the same name, so "Restart Terminal" means what it
    /// says — a clean process, not a resumed one. (Returning to a workspace
    /// that was merely backgrounded, not explicitly restarted, is the
    /// `materialize_node`/`-A` reconnect path; this function never takes
    /// that path.)
    pub fn restart_selected_terminals(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) {
        let ids: Vec<Uuid> = {
            let app_ref = app.borrow();
            app_ref
                .selected
                .iter()
                .copied()
                .filter(|id| {
                    app_ref
                        .nodes
                        .get(id)
                        .is_some_and(|entry| entry.record.as_terminal().is_some())
                })
                .collect()
        };
        for id in ids {
            let Some(terminal) = app
                .borrow()
                .nodes
                .get(&id)
                .and_then(|entry| entry.record.as_terminal())
                .cloned()
            else {
                continue;
            };
            environment::terminate(&mut app.borrow_mut().runtime, id, terminal.environment);
            let spawn_result = {
                let app_ref = app.borrow();
                build_terminal_launch(&app_ref, id, &terminal)
            }
            .and_then(|launch| {
                let prepared = environment::prepare_launch(terminal.environment, id, launch);
                app.borrow_mut()
                    .runtime
                    .spawn(id, terminal.cwd.clone(), prepared)
            });
            match spawn_result {
                Ok(()) => {
                    let mut app_mut = app.borrow_mut();
                    if let Some(entry) = app_mut.nodes.get_mut(&id) {
                        entry.exit_shown = false;
                        if let NodeWidget::Terminal(node) = &entry.widget {
                            node.status_label.set_text("");
                        }
                    }
                }
                Err(error) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!(
                        "couldn't restart {}: {error}",
                        terminal.name
                    )));
                }
            }
        }
    }

    // ---- Selection -----------------------------------------------------

    pub fn is_selected(&self, id: Uuid) -> bool {
        self.selected.contains(&id)
    }

    /// Applies `ids` as the new selection wholesale, updating the `selected`
    /// CSS class on every node so single-select, multi-select, marquee, and
    /// select-all/deselect-all all funnel through one place that keeps the
    /// visual state and the data in sync.
    fn set_selection(app: &Rc<RefCell<App>>, ids: HashSet<Uuid>) {
        let mut app_mut = app.borrow_mut();
        for (&id, entry) in &app_mut.nodes {
            if ids.contains(&id) {
                entry.widget.container().add_css_class("selected");
            } else {
                entry.widget.container().remove_css_class("selected");
            }
        }
        app_mut.selected = ids;
    }

    pub fn select_only(app: &Rc<RefCell<App>>, id: Uuid) {
        App::set_selection(app, HashSet::from([id]));
    }

    pub fn toggle_select(app: &Rc<RefCell<App>>, id: Uuid) {
        let mut ids = app.borrow().selected.clone();
        if !ids.remove(&id) {
            ids.insert(id);
        }
        App::set_selection(app, ids);
    }

    pub fn select_all(app: &Rc<RefCell<App>>) {
        let ids: HashSet<Uuid> = app.borrow().nodes.keys().copied().collect();
        App::set_selection(app, ids);
    }

    pub fn deselect_all(app: &Rc<RefCell<App>>) {
        App::set_selection(app, HashSet::new());
    }

    /// Called from a node's drag-begin, before the move itself starts:
    /// decides what the selection should be for this press. Clicking a node
    /// already part of a multi-selection keeps the whole selection (so
    /// dragging one of several selected cards moves all of them); clicking
    /// one that isn't selected replaces the selection with just it, unless
    /// `additive` (Ctrl/Shift held), which toggles it into/out of whatever
    /// was already selected.
    fn handle_node_press(app: &Rc<RefCell<App>>, id: Uuid, additive: bool) {
        if additive {
            App::toggle_select(app, id);
            return;
        }
        if !app.borrow().is_selected(id) {
            App::select_only(app, id);
        }
    }

    /// Replaces the selection with every node whose bounding box intersects
    /// the world-space rectangle spanned by `corner_a`/`corner_b` (in either
    /// order). Wired to `Canvas::connect_marquee_end`.
    pub fn apply_marquee_selection(
        app: &Rc<RefCell<App>>,
        corner_a: (f64, f64),
        corner_b: (f64, f64),
    ) {
        let (left, right) = (corner_a.0.min(corner_b.0), corner_a.0.max(corner_b.0));
        let (top, bottom) = (corner_a.1.min(corner_b.1), corner_a.1.max(corner_b.1));
        let ids: HashSet<Uuid> = app
            .borrow()
            .nodes
            .iter()
            .filter(|(_, entry)| {
                let (position, size) = (entry.record.position, entry.record.size);
                let (node_left, node_top) = position;
                let (node_right, node_bottom) =
                    (position.0 + size.0, position.1 + size.1 + TITLE_BAR_HEIGHT);
                node_left <= right && node_right >= left && node_top <= bottom && node_bottom >= top
            })
            .map(|(&id, _)| id)
            .collect();
        App::set_selection(app, ids);
    }

    // ---- Multi-node operations ------------------------------------------

    fn selected_entries(&self) -> Vec<(&Uuid, &NodeEntry)> {
        self.nodes
            .iter()
            .filter(|(id, _)| self.selected.contains(id))
            .collect()
    }

    /// Pushes `command` onto the undo stack and clears redo — every
    /// mutating, undoable action funnels through this so "any new edit
    /// invalidates the redo stack" lives in one place.
    fn push_undo(app: &Rc<RefCell<App>>, command: CanvasCommand) {
        let mut app_mut = app.borrow_mut();
        app_mut.undo_stack.push(command);
        app_mut.redo_stack.clear();
    }

    /// Deletes every selected node as one undo step.
    pub fn delete_selected(app: &Rc<RefCell<App>>) {
        let ids: Vec<Uuid> = app.borrow().selected.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        let (records, edges) = {
            let app_ref = app.borrow();
            let records: Vec<NodeRecord> = ids
                .iter()
                .filter_map(|id| app_ref.nodes.get(id).map(|e| e.record.clone()))
                .collect();
            let edges: Vec<EdgeRecord> = app_ref
                .edges
                .iter()
                .filter(|e| ids.contains(&e.source) || ids.contains(&e.target))
                .cloned()
                .collect();
            (records, edges)
        };
        for id in &ids {
            App::close_node(app, *id);
        }
        App::push_undo(
            app,
            CanvasCommand::RemoveNodes {
                nodes: records,
                edges,
            },
        );
        let _ = app.borrow().persist();
    }

    /// Duplicates every selected node, offset by `DUPLICATE_OFFSET`, as new
    /// nodes with fresh ids (and, for a `Terminal`, a freshly spawned
    /// process — duplicating a session duplicates its *configuration*, not
    /// its live conversation). Selects the duplicates and pushes one undo
    /// entry for the whole batch.
    pub fn duplicate_selected(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) {
        let sources: Vec<NodeRecord> = {
            let app_ref = app.borrow();
            app_ref
                .selected_entries()
                .into_iter()
                .map(|(_, e)| e.record.clone())
                .collect()
        };
        if sources.is_empty() {
            return;
        }
        let duplicates = duplicate_records(&sources);
        for record in &duplicates {
            let _ = materialize_node(app, record.clone(), toast_overlay);
        }
        App::set_selection(app, duplicates.iter().map(|r| r.id).collect());
        App::push_undo(
            app,
            CanvasCommand::AddNodes {
                nodes: duplicates,
                edges: Vec::new(),
            },
        );
        let _ = app.borrow().persist();
    }

    /// Copies every selected node's record into the in-canvas clipboard.
    pub fn copy_selected(app: &Rc<RefCell<App>>) {
        let records: Vec<NodeRecord> = {
            let app_ref = app.borrow();
            app_ref
                .selected_entries()
                .into_iter()
                .map(|(_, e)| e.record.clone())
                .collect()
        };
        app.borrow_mut().clipboard = records;
    }

    /// Pastes the clipboard as new nodes (fresh ids, offset position),
    /// selecting the pasted copies.
    pub fn paste_clipboard(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) {
        let clipboard = app.borrow().clipboard.clone();
        if clipboard.is_empty() {
            return;
        }
        let pasted = duplicate_records(&clipboard);
        for record in &pasted {
            let _ = materialize_node(app, record.clone(), toast_overlay);
        }
        App::set_selection(app, pasted.iter().map(|r| r.id).collect());
        App::push_undo(
            app,
            CanvasCommand::AddNodes {
                nodes: pasted,
                edges: Vec::new(),
            },
        );
        let _ = app.borrow().persist();
    }

    /// Toggles collapsed/expanded for every selected node as one undo step.
    /// `collapsed` is the target state, not a toggle, so a mixed selection
    /// converges on one state rather than each node flipping independently.
    pub fn set_selected_collapsed(app: &Rc<RefCell<App>>, collapsed: bool) {
        App::apply_property_batch(app, |record| record.collapsed = collapsed);
    }

    pub fn set_selected_locked(app: &Rc<RefCell<App>>, locked: bool) {
        App::apply_property_batch(app, |record| record.locked = locked);
    }

    fn apply_property_batch(app: &Rc<RefCell<App>>, edit: impl Fn(&mut NodeRecord)) {
        let ids: Vec<Uuid> = app.borrow().selected.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        let mut before = Vec::new();
        let mut after = Vec::new();
        {
            let mut app_mut = app.borrow_mut();
            for id in &ids {
                if let Some(entry) = app_mut.nodes.get_mut(id) {
                    before.push(entry.record.clone());
                    edit(&mut entry.record);
                    // Keeps the widget's actual on-screen collapsed state in
                    // sync with the record `edit` may have just changed — a
                    // no-op (checked inside `set_collapsed`) for a property
                    // edit (e.g. lock) that didn't touch `collapsed` at all.
                    entry.widget.set_collapsed(entry.record.collapsed);
                    after.push(entry.record.clone());
                }
            }
        }
        App::push_undo(app, CanvasCommand::SetProperties { before, after });
        let _ = app.borrow().persist();
    }

    /// Raises every selected node to the front (above every other node) —
    /// relative order among the selected nodes themselves is preserved.
    pub fn raise_selected(app: &Rc<RefCell<App>>) {
        let mut app_mut = app.borrow_mut();
        let canvas = app_mut.canvas.clone();
        let max_z = app_mut
            .nodes
            .values()
            .map(|e| e.record.z_order)
            .max()
            .unwrap_or(0);
        let ids: Vec<Uuid> = app_mut.selected.iter().copied().collect();
        for (offset, id) in ids.iter().enumerate() {
            if let Some(entry) = app_mut.nodes.get_mut(id) {
                entry.record.z_order = max_z + 1 + offset as i64;
                canvas.raise_node(entry.widget.container());
            }
        }
        drop(app_mut);
        App::schedule_persist(app);
    }

    /// Sends every selected node to the back (below every other node) —
    /// relative order among the selected nodes themselves is preserved, the
    /// same guarantee `raise_selected` makes. Plain forward iteration is
    /// what achieves that here, exactly as in `raise_selected`: each
    /// `canvas.lower_node` call inserts its widget as the Fixed container's
    /// new *first* (bottom-most) child, so processing the selection in its
    /// original order pushes each already-lowered node up by one slot as the
    /// next one is lowered beneath it — the first node processed ends up
    /// closest to the rest of the canvas, the last one processed ends up the
    /// very bottom, matching the ascending `z_order` values assigned in this
    /// same forward pass. (An earlier version of this function reversed the
    /// iteration to "compensate" for `lower_node` inserting at the opposite
    /// end from `raise_node` — that reasoning was backwards and inverted the
    /// group's relative order; see the code review that caught it.)
    pub fn lower_selected(app: &Rc<RefCell<App>>) {
        let mut app_mut = app.borrow_mut();
        let canvas = app_mut.canvas.clone();
        let min_z = app_mut
            .nodes
            .values()
            .map(|e| e.record.z_order)
            .min()
            .unwrap_or(0);
        let ids: Vec<Uuid> = app_mut.selected.iter().copied().collect();
        for (offset, id) in ids.iter().enumerate() {
            if let Some(entry) = app_mut.nodes.get_mut(id) {
                entry.record.z_order = min_z - 1 - offset as i64;
                canvas.lower_node(entry.widget.container());
            }
        }
        drop(app_mut);
        App::schedule_persist(app);
    }

    // ---- Layout tools ----------------------------------------------------

    fn selected_geometry(&self) -> Vec<(Uuid, NodeGeometry)> {
        self.selected_entries()
            .into_iter()
            .map(|(&id, entry)| (id, (entry.record.position, entry.record.size)))
            .collect()
    }

    /// Applies a pure `layout.rs` function to the selected nodes' geometry
    /// and writes the resulting positions back onto the live records/
    /// widgets, as one `MoveNodes` undo step.
    fn apply_layout(app: &Rc<RefCell<App>>, f: impl Fn(&mut [layout::Geometry])) {
        let pairs = app.borrow().selected_geometry();
        if pairs.len() < 2 {
            return;
        }
        let ids: Vec<Uuid> = pairs.iter().map(|(id, _)| *id).collect();
        let mut geometry: Vec<layout::Geometry> = pairs.iter().map(|(_, g)| *g).collect();
        let old_positions: Vec<(f64, f64)> = geometry.iter().map(|(p, _)| *p).collect();
        f(&mut geometry);

        let mut app_mut = app.borrow_mut();
        let canvas = app_mut.canvas.clone();
        let mut moves = Vec::new();
        for ((id, (new_position, _)), old_position) in ids.iter().zip(&geometry).zip(&old_positions)
        {
            if new_position == old_position {
                continue;
            }
            if let Some(entry) = app_mut.nodes.get_mut(id) {
                entry.record.position = *new_position;
                canvas.reposition_node(entry.widget.container(), *new_position);
                moves.push((*id, *old_position, *new_position));
            }
        }
        if moves.is_empty() {
            return;
        }
        app_mut.undo_stack.push(CanvasCommand::MoveNodes { moves });
        app_mut.redo_stack.clear();
        drop(app_mut);
        App::schedule_persist(app);
    }

    pub fn align_left(app: &Rc<RefCell<App>>) {
        App::apply_layout(app, layout::align_left);
    }
    pub fn align_right(app: &Rc<RefCell<App>>) {
        App::apply_layout(app, layout::align_right);
    }
    pub fn align_top(app: &Rc<RefCell<App>>) {
        App::apply_layout(app, layout::align_top);
    }
    pub fn align_bottom(app: &Rc<RefCell<App>>) {
        App::apply_layout(app, layout::align_bottom);
    }
    /// Below 3 selected nodes, `layout::distribute_horizontal` is a
    /// documented no-op (there's nothing "in between" two fixed ends to
    /// redistribute) — but a silent no-op reads indistinguishably from a
    /// broken command, so this surfaces a toast explaining why nothing
    /// moved instead of leaving `apply_layout` to do nothing quietly.
    pub fn distribute_horizontal(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) {
        if app.borrow().selected.len() < 3 {
            toast_overlay.add_toast(adw::Toast::new(
                "Select at least 3 nodes to distribute horizontally",
            ));
            return;
        }
        App::apply_layout(app, layout::distribute_horizontal);
    }
    /// Vertical counterpart of [`App::distribute_horizontal`].
    pub fn distribute_vertical(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) {
        if app.borrow().selected.len() < 3 {
            toast_overlay.add_toast(adw::Toast::new(
                "Select at least 3 nodes to distribute vertically",
            ));
            return;
        }
        App::apply_layout(app, layout::distribute_vertical);
    }

    // ---- Canvas navigation ------------------------------------------------

    /// Pans/zooms so every selected node (or, if none selected, every node)
    /// fits within `viewport_size` (screen pixels) with a small margin.
    fn zoom_to(app: &Rc<RefCell<App>>, geometry: Vec<layout::Geometry>, viewport_size: (f64, f64)) {
        let Some((min, max)) = layout::bounding_box(&geometry) else {
            return;
        };
        const MARGIN: f64 = 48.0;
        let (width, height) = (max.0 - min.0, max.1 - min.1);
        let available = (
            viewport_size.0 - MARGIN * 2.0,
            viewport_size.1 - MARGIN * 2.0,
        );
        if width <= 0.0 || height <= 0.0 || available.0 <= 0.0 || available.1 <= 0.0 {
            return;
        }
        let zoom = (available.0 / width)
            .min(available.1 / height)
            .clamp(0.1, 4.0);
        let center = ((min.0 + max.0) / 2.0, (min.1 + max.1) / 2.0);
        let app_ref = app.borrow();
        let mut state = app_ref.canvas.state.borrow_mut();
        state.zoom = zoom;
        state.pan = (
            viewport_size.0 / (2.0 * zoom) - center.0,
            viewport_size.1 / (2.0 * zoom) - center.1,
        );
        drop(state);
        app_ref.canvas.refresh_view();
    }

    pub fn zoom_to_selection(app: &Rc<RefCell<App>>, viewport_size: (f64, f64)) {
        let geometry: Vec<layout::Geometry> = app
            .borrow()
            .selected_entries()
            .into_iter()
            .map(|(_, e)| (e.record.position, e.record.size))
            .collect();
        if geometry.is_empty() {
            App::zoom_to_fit(app, viewport_size);
        } else {
            App::zoom_to(app, geometry, viewport_size);
        }
    }

    pub fn zoom_to_fit(app: &Rc<RefCell<App>>, viewport_size: (f64, f64)) {
        let geometry: Vec<layout::Geometry> = app
            .borrow()
            .nodes
            .values()
            .map(|e| (e.record.position, e.record.size))
            .collect();
        App::zoom_to(app, geometry, viewport_size);
    }

    // ---- Undo / redo -------------------------------------------------------

    pub fn undo(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) {
        let Some(command) = app.borrow_mut().undo_stack.pop() else {
            return;
        };
        App::apply_command(app, &command, true, toast_overlay);
        app.borrow_mut().redo_stack.push(command);
        let _ = app.borrow().persist();
    }

    pub fn redo(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) {
        let Some(command) = app.borrow_mut().redo_stack.pop() else {
            return;
        };
        App::apply_command(app, &command, false, toast_overlay);
        app.borrow_mut().undo_stack.push(command);
        let _ = app.borrow().persist();
    }

    /// Applies `command` forward (`inverse: false`, a redo) or backward
    /// (`inverse: true`, an undo) — one function for both directions since
    /// every variant's two directions are symmetric (add <-> remove, old
    /// position <-> new position, before <-> after).
    fn apply_command(
        app: &Rc<RefCell<App>>,
        command: &CanvasCommand,
        inverse: bool,
        toast_overlay: &adw::ToastOverlay,
    ) {
        match command {
            CanvasCommand::AddNodes { nodes, edges } => {
                if inverse {
                    for node in nodes {
                        App::close_node(app, node.id);
                    }
                } else {
                    for node in nodes {
                        let _ = materialize_node(app, node.clone(), toast_overlay);
                    }
                    app.borrow_mut().edges.extend(edges.iter().cloned());
                }
            }
            CanvasCommand::RemoveNodes { nodes, edges } => {
                if inverse {
                    for node in nodes {
                        let _ = materialize_node(app, node.clone(), toast_overlay);
                    }
                    app.borrow_mut().edges.extend(edges.iter().cloned());
                } else {
                    for node in nodes {
                        App::close_node(app, node.id);
                    }
                }
            }
            CanvasCommand::MoveNodes { moves } => {
                let mut app_mut = app.borrow_mut();
                let canvas = app_mut.canvas.clone();
                for (id, old_position, new_position) in moves {
                    let position = if inverse {
                        *old_position
                    } else {
                        *new_position
                    };
                    if let Some(entry) = app_mut.nodes.get_mut(id) {
                        entry.record.position = position;
                        canvas.reposition_node(entry.widget.container(), position);
                    }
                }
            }
            CanvasCommand::ResizeNode {
                id,
                old_size,
                new_size,
            } => {
                let size = if inverse { *old_size } else { *new_size };
                let mut app_mut = app.borrow_mut();
                if let Some(entry) = app_mut.nodes.get_mut(id) {
                    entry.record.size = size;
                    entry.widget.apply_resize(size);
                }
            }
            CanvasCommand::AddEdge { edge } => {
                let mut app_mut = app.borrow_mut();
                if inverse {
                    app_mut.edges.retain(|e| e.id != edge.id);
                } else {
                    app_mut.edges.push(edge.clone());
                }
            }
            CanvasCommand::RemoveEdge { edge } => {
                let mut app_mut = app.borrow_mut();
                if inverse {
                    app_mut.edges.push(edge.clone());
                } else {
                    app_mut.edges.retain(|e| e.id != edge.id);
                }
            }
            CanvasCommand::SetProperties { before, after } => {
                let records = if inverse { before } else { after };
                let mut app_mut = app.borrow_mut();
                for record in records {
                    if let Some(entry) = app_mut.nodes.get_mut(&record.id) {
                        entry.record = record.clone();
                        entry.widget.set_collapsed(entry.record.collapsed);
                    }
                }
            }
        }
    }
}

/// The next `z_order` to assign to a freshly-created node: one above the
/// current maximum, so new nodes always paint on top.
fn next_z_order(app: &App) -> i64 {
    app.nodes
        .values()
        .map(|e| e.record.z_order)
        .max()
        .unwrap_or(0)
        + 1
}

/// `(position, size)` for a node — what `canvas.rs`'s geometry helpers need,
/// without exposing the rest of `NodeRecord` to them.
fn node_geometry(entry: &NodeEntry) -> NodeGeometry {
    (entry.record.position, entry.record.size)
}

/// A node's display name for toast messages: a terminal's own name, or its
/// kind label for anything else.
fn node_display_name(app: &App, id: Uuid) -> String {
    app.nodes
        .get(&id)
        .map(|entry| match entry.record.as_terminal() {
            Some(terminal) => terminal.name.clone(),
            None => entry.record.kind.label().to_string(),
        })
        .unwrap_or_default()
}

/// Clones `records` with fresh ids and positions offset by
/// `DUPLICATE_OFFSET`, for `duplicate_selected`/`paste_clipboard`. A
/// `Terminal`'s pinned Claude session id is also regenerated — a duplicate
/// starts a new conversation, not a second process resuming the same one.
fn duplicate_records(records: &[NodeRecord]) -> Vec<NodeRecord> {
    records
        .iter()
        .map(|record| {
            let mut copy = record.clone();
            copy.id = Uuid::new_v4();
            copy.position = (
                record.position.0 + DUPLICATE_OFFSET.0,
                record.position.1 + DUPLICATE_OFFSET.1,
            );
            if let NodeKind::Terminal(terminal) = &mut copy.kind
                && terminal.claude_session_id.is_some()
            {
                terminal.claude_session_id = Some(Uuid::new_v4());
            }
            copy
        })
        .collect()
}

/// Click-to-rename: the title label swaps for an entry pre-filled with the
/// current name, Enter commits, Escape reverts without saving.
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
                return;
            }
            finish();
        }
    });

    let keys = gtk4::EventControllerKey::new();
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

/// Whether Ctrl or Shift is held in a gesture's current event — the
/// "additive selection" modifier, checked identically everywhere a node
/// press needs to know.
fn additive_modifier_held(gesture: &impl IsA<gtk4::Gesture>) -> bool {
    gesture
        .as_ref()
        .current_event()
        .map(|event| {
            let state = event.modifier_state();
            state.contains(gtk4::gdk::ModifierType::CONTROL_MASK)
                || state.contains(gtk4::gdk::ModifierType::SHIFT_MASK)
        })
        .unwrap_or(false)
}

/// Wires a node's drag-to-move (`drag_handle`), drag-to-resize
/// (`resize_handle`), and `close_button` — the chrome shared by every node
/// kind on the canvas, now that every kind shares one `NodeEntry`/
/// `NodeWidget` shape (Milestone 0's generic `CardEntry` trait is gone: there
/// is only one entry type to be generic over anymore).
///
/// A press on `drag_handle` also resolves node selection (`handle_node_press`)
/// before the move itself starts, and moves every currently-selected node
/// together, not just the one pressed. A locked node (`record.locked`) is
/// skipped by both move and resize.
fn wire_node_chrome(app: &Rc<RefCell<App>>, id: Uuid) {
    let (container, drag_handle, resize_handle, close_button, resizable_widget) = {
        let app_ref = app.borrow();
        let entry = app_ref.nodes.get(&id).expect("just inserted");
        (
            entry.widget.container().clone(),
            entry.widget.drag_handle().clone(),
            entry.widget.resize_handle().clone(),
            entry.widget.close_button().clone(),
            entry.widget.resizable_widget(),
        )
    };

    close_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::close_node(&app, id)
    });

    // (original positions of every selected node at drag start, pointer
    // position at drag start in the canvas `Fixed`'s stationary coordinate
    // space) — see `world_drag_delta` for why the pointer's start must be
    // captured in that frame.
    let move_start: MoveStart = Rc::new(RefCell::new(None));
    let drag = gtk4::GestureDrag::new();
    drag.connect_drag_begin({
        let app = Rc::clone(app);
        let container = container.clone();
        let move_start = Rc::clone(&move_start);
        move |gesture, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            App::handle_node_press(&app, id, additive_modifier_held(gesture));

            let app_mut = app.borrow();
            let pointer = canvas_point(gesture, &app_mut.canvas.fixed, (x, y));
            let app_for_raise = app.clone();
            let container_for_raise = container.clone();
            glib::idle_add_local_once(move || {
                app_for_raise
                    .borrow()
                    .canvas
                    .raise_node(&container_for_raise);
            });
            let selected: Vec<Uuid> = app_mut.selected.iter().copied().collect();
            let positions: HashMap<Uuid, (f64, f64)> = selected
                .into_iter()
                .filter_map(|sid| {
                    app_mut.nodes.get(&sid).and_then(|entry| {
                        (!entry.record.locked).then_some((sid, entry.record.position))
                    })
                })
                .collect();
            *move_start.borrow_mut() = pointer.map(|p| (positions, p));
        }
    });
    drag.connect_drag_update({
        let app = Rc::clone(app);
        let move_start = Rc::clone(&move_start);
        move |gesture, _offset_x, _offset_y| {
            let Some((start_positions, start_pointer)) = move_start.borrow().clone() else {
                return;
            };
            let delta = {
                let app_ref = app.borrow();
                let zoom = app_ref.canvas.state.borrow().zoom;
                match world_drag_delta(gesture, &app_ref.canvas.fixed, start_pointer, zoom) {
                    Some(delta) => delta,
                    None => return,
                }
            };
            let mut app_mut = app.borrow_mut();
            let canvas = app_mut.canvas.clone();
            for (sid, start_position) in &start_positions {
                let new_position = (start_position.0 + delta.0, start_position.1 + delta.1);
                if let Some(entry) = app_mut.nodes.get_mut(sid) {
                    entry.record.position = new_position;
                    canvas.reposition_node(entry.widget.container(), new_position);
                }
            }
        }
    });
    // Persisting (and recording the undo step) is deliberately NOT done in
    // `drag-update` above: that fires on every single pointer-motion tick
    // during the drag, and doing either on the hottest possible path made
    // dragging visibly stutter. Both happen once, here, at drag-end — which
    // is also what makes a whole drag ONE undo step rather than hundreds.
    drag.connect_drag_end({
        let app = Rc::clone(app);
        let move_start = Rc::clone(&move_start);
        move |_gesture, _x, _y| {
            let Some((start_positions, _)) = move_start.borrow_mut().take() else {
                return;
            };
            let mut app_mut = app.borrow_mut();
            let canvas = app_mut.canvas.clone();
            let snap = app_mut.snap_to_grid;
            let mut moves = Vec::new();
            for (sid, old_position) in &start_positions {
                let Some(entry) = app_mut.nodes.get_mut(sid) else {
                    continue;
                };
                let mut new_position = entry.record.position;
                if snap {
                    use crate::canvas::GRID_MINOR;
                    new_position = (
                        (new_position.0 / GRID_MINOR).round() * GRID_MINOR,
                        (new_position.1 / GRID_MINOR).round() * GRID_MINOR,
                    );
                    entry.record.position = new_position;
                    canvas.reposition_node(entry.widget.container(), new_position);
                }
                if new_position != *old_position {
                    moves.push((*sid, *old_position, new_position));
                }
            }
            if !moves.is_empty() {
                app_mut.undo_stack.push(CanvasCommand::MoveNodes { moves });
                app_mut.redo_stack.clear();
            }
            drop(app_mut);
            App::schedule_persist(&app);
        }
    });
    drag_handle.add_controller(drag);

    // Same shape as `move_start` above: resizing a card moves its own
    // bottom-right grip, so the gesture's raw offsets suffer the identical
    // feedback described in `world_drag_delta`. Resize is deliberately
    // single-node even within a multi-selection — there's no well-defined
    // "resize everyone together" semantics the milestone asked for.
    let resize_start: ResizeStart = Rc::new(RefCell::new(None));
    let resize = gtk4::GestureDrag::new();
    resize.connect_drag_begin({
        let app = Rc::clone(app);
        let resizable_widget = resizable_widget.clone();
        let resize_start = Rc::clone(&resize_start);
        move |gesture, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let app_mut = app.borrow();
            let pointer = canvas_point(gesture, &app_mut.canvas.fixed, (x, y));
            let locked = app_mut.nodes.get(&id).is_some_and(|e| e.record.locked);
            let fallback_size = app_mut.nodes.get(&id).map(|e| e.record.size);
            *resize_start.borrow_mut() = match (locked, fallback_size, pointer) {
                (false, Some(fallback_size), Some(pointer)) => Some((
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
            let Some(entry) = app_mut.nodes.get_mut(&id) else {
                return;
            };
            entry.record.size = new_size;
            entry.widget.apply_resize(new_size);
        }
    });
    resize.connect_drag_end({
        let app = Rc::clone(app);
        let resizable_widget = resizable_widget.clone();
        let resize_start = Rc::clone(&resize_start);
        move |_gesture, _x, _y| {
            let Some((old_size, _)) = resize_start.borrow_mut().take() else {
                return;
            };
            if let Some(size) = allocated_size(&resizable_widget) {
                let mut app_mut = app.borrow_mut();
                if let Some(entry) = app_mut.nodes.get_mut(&id) {
                    entry.record.size = size;
                    if size != old_size {
                        app_mut.undo_stack.push(CanvasCommand::ResizeNode {
                            id,
                            old_size,
                            new_size: size,
                        });
                        app_mut.redo_stack.clear();
                    }
                }
            }
            App::schedule_persist(&app);
        }
    });
    resize_handle.add_controller(resize);
}

/// Wires a terminal node's link button (click to enter link mode, sourced
/// from this node) and its terminal (click to complete a pending link,
/// targeting this node). Edge-creation UI is deliberately `Terminal`-only
/// for now, matching the pre-Milestone-1 behavior exactly (the `EdgeRecord`
/// model itself is generic over any two node ids — `App::create_edge`
/// doesn't care what kind either endpoint is — only the UI affordance to
/// start one is scoped to sessions today).
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
                "link mode: click another node to connect this one's output into it",
            ));
        }
    });

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

/// Builds the right `NodeWidget` for `record.kind`, wires its change
/// tracking (persist on edit) plus the shared chrome (`wire_node_chrome`),
/// adds it to the canvas, and inserts the resulting `NodeEntry` into
/// `app.nodes`. The single creation pathway used by `create_session`/
/// `create_note`/`create_text_node`/`create_placeholder_node`,
/// `spawn_workspace_contents` (restore/switch), and undo/redo re-adding a
/// removed node — so "what it takes to make a `NodeRecord` live" is written
/// once instead of five times.
fn materialize_node(
    app: &Rc<RefCell<App>>,
    record: NodeRecord,
    toast_overlay: &adw::ToastOverlay,
) -> Result<(), String> {
    let id = record.id;
    let position = record.position;

    let widget = match &record.kind {
        NodeKind::Terminal(terminal) => {
            // If this id is already running (the workspace was backgrounded,
            // not terminated, by `detach_active_workspace`, or a restart's
            // tmux reconnect already happened for it earlier in this same
            // restore pass), reattach to it instead of spawning a second
            // process under the same id — the core of switching workspaces
            // without killing or duplicating anything.
            if !app.borrow().runtime.is_alive(id) {
                let launch = {
                    let app_ref = app.borrow();
                    build_terminal_launch(&app_ref, id, terminal)
                        .map_err(|error| format!("couldn't restore {}: {error}", terminal.name))?
                };
                let prepared = environment::prepare_launch(terminal.environment, id, launch);
                app.borrow_mut()
                    .runtime
                    .spawn(id, terminal.cwd.clone(), prepared)
                    .map_err(|error| format!("couldn't restore {}: {error}", terminal.name))?;
            }
            let node = SessionNode::new(&terminal.name, record.collapsed, {
                let app = Rc::clone(app);
                move |collapsed| {
                    if let Some(entry) = app.borrow_mut().nodes.get_mut(&id) {
                        entry.record.collapsed = collapsed;
                    }
                    App::schedule_persist(&app);
                }
            });
            node.request_grid(record.size.0, record.size.1);
            apply_role_badge(&app.borrow(), &node, terminal.role_id);
            node.connect_commit({
                let app = Rc::clone(app);
                move |bytes| {
                    let _ = app.borrow_mut().runtime.write_input(id, bytes);
                }
            });
            wire_link_controls(app, &node, id, toast_overlay);
            wire_rename(app, &node, id, toast_overlay);
            NodeWidget::Terminal(node)
        }
        NodeKind::Note(note) => {
            let node = NoteNode::new(
                &note.markdown,
                &note.color,
                note.view_mode,
                record.collapsed,
                {
                    let app = Rc::clone(app);
                    move |collapsed| {
                        if let Some(entry) = app.borrow_mut().nodes.get_mut(&id) {
                            entry.record.collapsed = collapsed;
                        }
                        App::schedule_persist(&app);
                    }
                },
            );
            node.edit_view.buffer().connect_changed({
                let app = Rc::clone(app);
                move |buffer| {
                    let markdown = crate::node::buffer_text(buffer);
                    if let Some(entry) = app.borrow_mut().nodes.get_mut(&id)
                        && let Some(note) = entry.record.as_note_mut()
                    {
                        note.markdown = markdown;
                    }
                    App::schedule_persist(&app);
                }
            });
            node.mode_button.connect_clicked({
                let app = Rc::clone(app);
                move |_| {
                    let mut app_mut = app.borrow_mut();
                    let Some(entry) = app_mut.nodes.get_mut(&id) else {
                        return;
                    };
                    let NodeWidget::Note(note_node) = &entry.widget else {
                        return;
                    };
                    let Some(note) = entry.record.as_note_mut() else {
                        return;
                    };
                    note.view_mode = match note.view_mode {
                        NoteViewMode::Edit => NoteViewMode::Preview,
                        NoteViewMode::Preview => NoteViewMode::Split,
                        NoteViewMode::Split => NoteViewMode::Edit,
                    };
                    note_node.set_view_mode(note.view_mode);
                    drop(app_mut);
                    App::schedule_persist(&app);
                }
            });
            NodeWidget::Note(node)
        }
        NodeKind::Text(text) => {
            let node = TextNode::new(&text.content, record.collapsed, {
                let app = Rc::clone(app);
                move |collapsed| {
                    if let Some(entry) = app.borrow_mut().nodes.get_mut(&id) {
                        entry.record.collapsed = collapsed;
                    }
                    App::schedule_persist(&app);
                }
            });
            node.text_view.buffer().connect_changed({
                let app = Rc::clone(app);
                move |buffer| {
                    let content = crate::node::buffer_text(buffer);
                    if let Some(entry) = app.borrow_mut().nodes.get_mut(&id)
                        && let NodeKind::Text(text) = &mut entry.record.kind
                    {
                        text.content = content;
                    }
                    App::schedule_persist(&app);
                }
            });
            NodeWidget::Text(node)
        }
        NodeKind::FileTree(_) | NodeKind::Portal(_) | NodeKind::Drawing(_) | NodeKind::Group(_) => {
            let detail = match &record.kind {
                NodeKind::FileTree(payload) if !payload.root_label.is_empty() => {
                    payload.root_label.clone()
                }
                NodeKind::Portal(payload) if !payload.url.is_empty() => payload.url.clone(),
                NodeKind::Group(payload) if !payload.label.is_empty() => payload.label.clone(),
                _ => "Not implemented yet".to_string(),
            };
            let node = PlaceholderNode::new(record.kind.label(), &detail, record.collapsed, {
                let app = Rc::clone(app);
                move |collapsed| {
                    if let Some(entry) = app.borrow_mut().nodes.get_mut(&id) {
                        entry.record.collapsed = collapsed;
                    }
                    App::schedule_persist(&app);
                }
            });
            NodeWidget::Placeholder(node)
        }
    };

    {
        let app_ref = app.borrow();
        app_ref.canvas.add_node(widget.container(), position);
    }
    // Inserted before wiring chrome, not after: `wire_node_chrome` looks
    // `id` up in `app.nodes` (to read its widget handles) the moment it's
    // called, not just when a gesture later fires, so the entry has to exist
    // first.
    app.borrow_mut().nodes.insert(
        id,
        NodeEntry {
            record,
            widget,
            pty_grid: None,
            exit_shown: false,
        },
    );
    wire_node_chrome(app, id);
    Ok(())
}

/// Spawns one `NodeEntry` per record into the (already-cleared) canvas via
/// `materialize_node`, then sets `edges` as the active workspace's edge
/// list. Shared by every path that makes a workspace's saved records live.
/// Spawn failures are collected and returned rather than aborting the rest
/// of the workspace.
fn spawn_workspace_contents(
    app: &Rc<RefCell<App>>,
    mut nodes: Vec<NodeRecord>,
    edges: Vec<EdgeRecord>,
    toast_overlay: &adw::ToastOverlay,
) -> Vec<String> {
    // Materializing in persisted (insertion) order would silently forget
    // whatever front/back stacking "raise_selected"/"lower_selected" left in
    // place, since the canvas's paint order is simply each node's add_node
    // call order — sorting by the same z_order those commands maintain is
    // what makes "send to back" (etc.) survive a restart rather than only
    // lasting until the app is closed.
    nodes.sort_by_key(|record| record.z_order);
    let mut errors = Vec::new();
    for record in nodes {
        let label = record.kind.label().to_string();
        if let Err(error) = materialize_node(app, record, toast_overlay) {
            errors.push(format!("couldn't restore {label}: {error}"));
        }
    }
    app.borrow_mut().edges = edges;
    errors
}

/// Resolves a Claude account's isolated config directory for a Claude-kind
/// agent, falling back to the default account when none was picked; `None`
/// for every other agent.
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

/// Builds the `Launch` for (re)starting a `Terminal` node's process from its
/// persisted payload — account resolution and the resume flag, shared by
/// `materialize_node` (first spawn, restore, and background reattach) and
/// `restart_selected_terminals` (explicit respawn) so that logic lives in
/// exactly one place. Does NOT apply `environment::prepare_launch` — callers
/// do that themselves, since which environment applies differs by caller
/// (always `terminal.environment` today, but keeping it a separate step
/// keeps this function usable for a future override).
fn build_terminal_launch(
    app: &App,
    id: Uuid,
    terminal: &TerminalPayload,
) -> anyhow::Result<Launch> {
    let claude = resolve_claude_account(app, &terminal.agent, terminal.claude_account.clone())?;
    let resume = match &terminal.agent {
        Agent::Claude => terminal.claude_session_id.is_some(),
        _ => true,
    };
    Ok(with_session_env(
        terminal.agent.launch(LaunchRequest {
            resume,
            claude_session_id: terminal.claude_session_id,
            claude_config_dir: claude.as_ref().map(|(_, dir)| dir.as_path()),
            ..Default::default()
        }),
        id,
    ))
}

/// Looks up `role_id` and updates `node`'s title-bar badge to match.
fn apply_role_badge(app: &App, node: &SessionNode, role_id: Option<Uuid>) {
    let role = role_id.and_then(|id| app.find_role(id));
    node.set_role(
        role.as_ref().map(|role| role.name.as_str()),
        role.as_ref().and_then(|role| role.icon.as_deref()),
        role.as_ref().and_then(|role| role.accent.as_deref()),
    );
}

#[allow(clippy::too_many_arguments)]
fn build_terminal_record(
    app: &App,
    name: &str,
    cwd: &Path,
    agent: Agent,
    claude_account: Option<String>,
    role_id: Option<Uuid>,
    position: (f64, f64),
) -> anyhow::Result<NodeRecord> {
    if let Agent::Custom { program, .. } = &agent
        && program.trim().is_empty()
    {
        anyhow::bail!("give the custom command a program to run");
    }
    let claude = resolve_claude_account(app, &agent, claude_account)?;
    let claude_session_id = claude.is_some().then(Uuid::new_v4);
    let session_id = Uuid::new_v4();
    let terminal = TerminalPayload {
        name: name.to_string(),
        cwd: cwd.to_path_buf(),
        claude_account: claude.map(|(account, _)| account),
        claude_session_id,
        agent,
        role_id,
        environment: app.workspace_environment,
    };
    Ok(NodeRecord {
        id: session_id,
        floor: FloorRef::Ground,
        position,
        size: (720.0, 504.0),
        z_order: next_z_order(app),
        collapsed: false,
        locked: false,
        kind: NodeKind::Terminal(terminal),
    })
}

/// The four placeholder node kinds `main.rs`'s "New node" picker offers,
/// each with an empty default payload — real content (a file-tree root, a
/// portal URL, ...) is a later milestone's job to fill in.
pub fn placeholder_kind(label: &str) -> Option<NodeKind> {
    match label {
        "File Tree" => Some(NodeKind::FileTree(Default::default())),
        "Portal" => Some(NodeKind::Portal(Default::default())),
        "Drawing" => Some(NodeKind::Drawing(Default::default())),
        "Group" => Some(NodeKind::Group(GroupPayload::default())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_record() -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (200.0, 100.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Text(TextPayload {
                content: "hi".to_string(),
            }),
        }
    }

    fn note_record() -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (200.0, 100.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Note(NotePayload {
                markdown: "# hi\n- one\n- two".to_string(),
                color: "yellow".to_string(),
                view_mode: NoteViewMode::Preview,
            }),
        }
    }

    /// Regression test: `materialize_node` used to call `wire_node_chrome`
    /// *before* inserting the new entry into `app.nodes`, even though
    /// `wire_node_chrome` looks `id` up in `app.nodes` immediately (not only
    /// once a gesture fires) — so creating or restoring *any* node beyond an
    /// empty workspace panicked deterministically. Caught by manual
    /// end-to-end testing (unit tests alone never exercise live GTK
    /// wiring); this pins the fix. Needs a display, so excluded from the
    /// default `cargo test` run — see `node.rs`'s own
    /// `request_grid_drives_the_cards_real_allocation` for the same pattern.
    #[test]
    #[ignore = "needs a display"]
    fn materialize_node_inserts_before_wiring_chrome() {
        if gtk4::init().is_err() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
        let app = App::new(
            AccountStore::new(tmp.join("accounts")),
            tmp.join("store.json"),
        );
        let toast_overlay = adw::ToastOverlay::new();

        let text = text_record();
        materialize_node(&app, text.clone(), &toast_overlay).unwrap();
        assert!(app.borrow().nodes.contains_key(&text.id));

        let note = note_record();
        materialize_node(&app, note.clone(), &toast_overlay).unwrap();
        assert!(app.borrow().nodes.contains_key(&note.id));
    }

    /// Regression test for a RefCell double-borrow panic found in review: a
    /// `Note` node's `edit_view` buffer-changed handler held an
    /// `app.borrow_mut()` live (via an `if let` scrutinee's extended
    /// temporary lifetime) while calling a helper that itself did
    /// `app.borrow()` — "already mutably borrowed" on every keystroke, which
    /// crashed instantly since `create_note` defaults a new note to Edit
    /// mode. Typing into the live `edit_view` buffer (as a user keystroke
    /// would) is what actually exercises this path; the sibling test above
    /// only materializes the node without touching its buffer, so it alone
    /// would not have caught this.
    #[test]
    #[ignore = "needs a display"]
    fn typing_into_a_note_does_not_panic() {
        if gtk4::init().is_err() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
        let app = App::new(
            AccountStore::new(tmp.join("accounts")),
            tmp.join("store.json"),
        );
        let toast_overlay = adw::ToastOverlay::new();

        let mut note = note_record();
        note.kind = NodeKind::Note(NotePayload {
            markdown: String::new(),
            color: "yellow".to_string(),
            view_mode: NoteViewMode::Edit,
        });
        let id = note.id;
        materialize_node(&app, note, &toast_overlay).unwrap();

        let edit_view = {
            let app_ref = app.borrow();
            let NodeWidget::Note(note_node) = &app_ref.nodes.get(&id).unwrap().widget else {
                panic!("expected a Note widget");
            };
            note_node.edit_view.clone()
        };
        // Triggers `connect_changed` exactly as a keystroke would.
        edit_view.buffer().set_text("hello");

        assert_eq!(
            app.borrow()
                .nodes
                .get(&id)
                .unwrap()
                .record
                .as_note()
                .unwrap()
                .markdown,
            "hello"
        );
    }

    fn terminal_node(environment: EnvironmentKind) -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (480.0, 320.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Terminal(TerminalPayload {
                name: "t".to_string(),
                cwd: std::env::temp_dir(),
                agent: Agent::Shell,
                claude_session_id: None,
                claude_account: None,
                role_id: None,
                environment,
            }),
        }
    }

    fn sleeper_launch() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 5".to_string()],
            envs: vec![],
        }
    }

    fn dormant_workspace(name: &str, nodes: Vec<NodeRecord>) -> WorkspaceRecord {
        WorkspaceRecord {
            id: Uuid::new_v4(),
            name: name.to_string(),
            root_dir: std::env::temp_dir(),
            nodes,
            edges: Vec::new(),
            canvas: CanvasRecord::default(),
            environment: EnvironmentKind::LocalPty,
            color: None,
            icon: None,
            created_at: 0,
            last_opened: 0,
        }
    }

    /// Registry-level (no GTK canvas interaction) exercise of the exact
    /// scenario Milestone 2 exists for: a dormant workspace's terminal is
    /// genuinely running (`runtime.spawn` directly, standing in for what
    /// `switch_workspace` would have done through `materialize_node`), is
    /// reported as `Background`, and `unload_workspace` kills only that
    /// workspace's own sessions — a sibling dormant workspace's session, and
    /// anything in the active workspace, must survive untouched. This is the
    /// cross-workspace process isolation guarantee requirement #7 in
    /// `steps.md` requires.
    #[test]
    #[ignore = "needs a display"]
    fn unload_workspace_only_terminates_that_workspaces_own_sessions() {
        if gtk4::init().is_err() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
        let app = App::new(
            AccountStore::new(tmp.join("accounts")),
            tmp.join("store.json"),
        );

        let node_a = terminal_node(EnvironmentKind::LocalPty);
        let node_b = terminal_node(EnvironmentKind::LocalPty);
        let active_node = terminal_node(EnvironmentKind::LocalPty);
        {
            let mut app_mut = app.borrow_mut();
            app_mut
                .runtime
                .spawn(node_a.id, std::env::temp_dir(), sleeper_launch())
                .unwrap();
            app_mut
                .runtime
                .spawn(node_b.id, std::env::temp_dir(), sleeper_launch())
                .unwrap();
            app_mut
                .runtime
                .spawn(active_node.id, std::env::temp_dir(), sleeper_launch())
                .unwrap();
            app_mut
                .inactive_workspaces
                .push(dormant_workspace("workspace-a", vec![node_a.clone()]));
            app_mut
                .inactive_workspaces
                .push(dormant_workspace("workspace-b", vec![node_b.clone()]));
        }

        assert_eq!(
            app.borrow()
                .workspace_runtime_state(app.borrow().inactive_workspaces[0].id),
            WorkspaceRuntimeState::Background
        );

        let workspace_a_id = app.borrow().inactive_workspaces[0].id;
        App::unload_workspace(&app, workspace_a_id).unwrap();

        assert!(!app.borrow().runtime.is_alive(node_a.id));
        assert!(app.borrow().runtime.is_alive(node_b.id));
        assert!(app.borrow().runtime.is_alive(active_node.id));
        assert_eq!(
            app.borrow().workspace_runtime_state(workspace_a_id),
            WorkspaceRuntimeState::Unloaded
        );

        app.borrow_mut().runtime.terminate(node_b.id);
        app.borrow_mut().runtime.terminate(active_node.id);
    }

    #[test]
    #[ignore = "needs a display"]
    fn unload_workspace_refuses_on_the_active_workspace() {
        if gtk4::init().is_err() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
        let app = App::new(
            AccountStore::new(tmp.join("accounts")),
            tmp.join("store.json"),
        );
        let active_id = app.borrow().workspace_id;
        assert!(App::unload_workspace(&app, active_id).is_err());
    }

    /// `workspace_runtime_state` must report `Unloaded` for a dormant
    /// workspace whose terminals were never spawned (e.g. right after
    /// `unload_workspace`, or a workspace that's simply never been visited
    /// since app start) and `Active` for whichever workspace `App` currently
    /// considers live, independent of the runtime registry.
    #[test]
    #[ignore = "needs a display"]
    fn workspace_runtime_state_distinguishes_active_background_and_unloaded() {
        if gtk4::init().is_err() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
        let app = App::new(
            AccountStore::new(tmp.join("accounts")),
            tmp.join("store.json"),
        );
        let active_id = app.borrow().workspace_id;
        assert_eq!(
            app.borrow().workspace_runtime_state(active_id),
            WorkspaceRuntimeState::Active
        );

        let node = terminal_node(EnvironmentKind::LocalPty);
        let dormant = dormant_workspace("idle", vec![node]);
        let dormant_id = dormant.id;
        app.borrow_mut().inactive_workspaces.push(dormant);
        assert_eq!(
            app.borrow().workspace_runtime_state(dormant_id),
            WorkspaceRuntimeState::Unloaded
        );

        assert_eq!(
            app.borrow().workspace_runtime_state(Uuid::new_v4()),
            WorkspaceRuntimeState::Unloaded
        );
    }
}
