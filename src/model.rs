//! The generalized canvas domain model: every object on the canvas is a
//! [`NodeRecord`] (a stable id plus shared position/size/layering/collapsed/
//! locked metadata, and a [`NodeKind`]-specific payload), and every
//! connection between two nodes is an [`EdgeRecord`] (stable source/target
//! ids plus a set of capabilities, empty meaning visual-only). Both are pure
//! data — no GTK, no runtime handles — persisted inside a `WorkspaceRecord`
//! (`store.rs`) and migrated by `migration.rs`.
//!
//! This module intentionally does not implement orchestration permission
//! behavior for edge capabilities, portal/drawing functionality, or group
//! containment — those are later milestones. The payload types here exist
//! so the model is genuinely generic now, not because their owning features
//! are built. (`FileTree`, `Editor` and file-backed `Note`s became real in
//! Milestone 6 — their payloads hold only persisted *view* state and
//! project-relative paths; file contents, Git status and live watchers are
//! runtime state owned by `app`/`project`, never serialized here.)

use crate::agent::Agent;
use crate::project::git::DiffScope;
use crate::project::sync::FileRevision;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use uuid::Uuid;

/// Which runtime a terminal node's process runs under. Persisted on
/// [`TerminalPayload`] (per-terminal) and as a default on
/// `store::WorkspaceRecord` (new terminals in that workspace start with the
/// workspace's own default unless the new-session dialog is given a reason
/// to override it — no such override UI exists yet, so today every new
/// terminal simply inherits its workspace's default). A pure data enum
/// deliberately kept here rather than in `environment.rs`: that module is
/// the runtime/process-spawning layer (it shells out to `tmux`), and this
/// crate's dependency direction keeps the domain model independent of it —
/// `environment.rs` depends on this type, not the other way around.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EnvironmentKind {
    #[default]
    LocalPty,
    LocalTmux,
}

/// Which floor a node lives on. `Ground` is the default and, until Milestone
/// 9 introduces git-isolated floors, the only value that ever occurs.
/// Present on every `NodeRecord` from the start specifically so Milestone 9
/// needs no node migration (see `.claude/steps.md`'s "Adjustments to settle
/// before PR 1").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FloorRef {
    #[default]
    Ground,
    Floor(Uuid),
}

/// A session's provider-specific data, everything `SessionRecord` used to
/// hold besides its id/position/size (now on the owning `NodeRecord`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalPayload {
    pub name: String,
    pub cwd: PathBuf,
    pub agent: Agent,
    pub claude_session_id: Option<Uuid>,
    pub claude_account: Option<String>,
    /// `true` until this terminal's process has actually been spawned for
    /// the very first time (freshly created, duplicated, or handed off to —
    /// never on restore/reattach/restart of one that has run before). Two
    /// things key off it:
    ///
    /// - For Claude specifically, whether `build_terminal_launch` picks
    ///   `--session-id` (create) over `--resume` (continue). Without this,
    ///   a brand-new id with `claude_session_id.is_some()` but no real
    ///   conversation behind it would launch with `--resume` and fail with
    ///   "No conversation found with ID ...".
    /// - For every provider, whether a launch sends any discovery/skill
    ///   prompt at all. An agent that has already launched once already
    ///   knows how to reach `duetctl` (Claude's `duet` skill is installed
    ///   once and persists; a resumed Codex/other conversation still has it
    ///   in its own history) — resending it on every reattach/restore would
    ///   just add a stray synthetic turn to an otherwise real conversation
    ///   every time `duet` itself restarts.
    ///
    /// `#[serde(default)]` so every terminal saved before this field
    /// existed loads as `false` — the safe assumption for a session that,
    /// by virtue of already being persisted, has necessarily launched
    /// before.
    #[serde(default)]
    pub never_launched: bool,
    #[serde(default)]
    pub role_id: Option<Uuid>,
    /// Which runtime this terminal's process runs under. `#[serde(default)]`
    /// so an older-schema terminal (every one saved before Milestone 2)
    /// loads as `LocalPty` — the only backend that existed then, and still
    /// the default for a newly created terminal.
    #[serde(default)]
    pub environment: EnvironmentKind,
}

/// A note's content: Markdown source (plain text is valid Markdown, so a
/// migrated sticky note's text becomes its source unchanged) plus the same
/// pastel color tint sticky notes have always had.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotePayload {
    pub markdown: String,
    pub color: String,
    /// Whether this note is showing rendered Markdown or raw source.
    /// Per-node, not global, so flipping one note's mode doesn't affect
    /// others. `#[serde(default)]` so a migrated note opens in Preview.
    #[serde(default = "NoteViewMode::default_mode")]
    pub view_mode: NoteViewMode,
    /// `Some` for a file-backed note (Milestone 6): `markdown` is then a
    /// synced copy of a project file, kept in step with it by
    /// `App::sync_project_files`. `None` (the default, and every note saved
    /// before Milestone 6) is an ordinary internal note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<NoteFileBacking>,
}

/// Which project file a file-backed note mirrors, and the revision both
/// last agreed on — persisted so an edit made to the file while Duet was
/// closed (or while this note's workspace was dormant) is detected on the
/// next sync instead of being overwritten. See `project::sync::reconcile`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteFileBacking {
    /// Project-relative (`project::ProjectPath` syntax). Kept as a plain
    /// string, parsed at use, so a path that somehow stopped being valid
    /// can never make the whole store fail to load.
    pub path: String,
    /// The last revision synced in either direction; `None` until the
    /// first successful sync.
    #[serde(default)]
    pub revision: Option<FileRevision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteViewMode {
    Edit,
    Preview,
    Split,
}

impl NoteViewMode {
    fn default_mode() -> Self {
        NoteViewMode::Preview
    }
}

/// A plain-text annotation node — simpler than a `Note`: no Markdown
/// rendering, no Edit/Preview split, just a label of text on the canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextPayload {
    pub content: String,
}

/// A FileTree node's persisted view state (Milestone 6). Every field is
/// `#[serde(default)]`, so a placeholder FileTree saved before Milestone 6
/// (which only had `root_label`) loads as a real tree of its workspace's
/// project root. Several FileTree nodes can coexist, each with its own
/// state; directory listings and Git status are runtime-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileTreePayload {
    /// Optional display label (the pre-Milestone-6 placeholder's only
    /// field); the root path's own name is shown when empty.
    #[serde(default)]
    pub root_label: String,
    /// The directory this tree is rooted at, project-relative (`""` is the
    /// project root). Strings rather than `ProjectPath`s for the same
    /// reason as `NoteFileBacking::path`.
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub expanded: BTreeSet<String>,
    #[serde(default)]
    pub selected: Option<String>,
    #[serde(default)]
    pub show_hidden: bool,
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
    #[serde(default = "default_true")]
    pub show_git_status: bool,
}

fn default_true() -> bool {
    true
}

impl Default for FileTreePayload {
    fn default() -> Self {
        FileTreePayload {
            root_label: String::new(),
            root: String::new(),
            expanded: BTreeSet::new(),
            selected: None,
            show_hidden: false,
            respect_gitignore: true,
            show_git_status: true,
        }
    }
}

/// An embedded editor (or read-only diff view) of one project file
/// (Milestone 6). Only *which* file and view are persisted; the buffer is
/// loaded from the file on open, and unsaved edits are runtime state the
/// editor itself guards (explicit save, conflict-checked).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditorPayload {
    pub path: String,
    /// `Some` shows the file's Git diff in that scope instead of its source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffScope>,
}

/// Placeholder for Milestone 8's browser portal node.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PortalPayload {
    pub url: String,
}

/// Placeholder for a future freehand drawing node. No strokes yet.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DrawingPayload {}

/// Placeholder for a future visual grouping node. Multi-select-and-move
/// (this milestone's selection system) already covers "move several nodes
/// together"; this type exists only so `Group` is a representable node kind,
/// not because grouping/containment semantics are implemented.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GroupPayload {
    pub label: String,
}

/// What kind of object a `NodeRecord` is, and that kind's own data. Matching
/// on this is how every node-kind-specific behavior (spawning a process,
/// rendering Markdown, ...) dispatches — `app.rs`/`node.rs` own that
/// dispatch, this module only owns the data shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum NodeKind {
    Terminal(TerminalPayload),
    Note(NotePayload),
    Text(TextPayload),
    FileTree(FileTreePayload),
    Portal(PortalPayload),
    Drawing(DrawingPayload),
    Group(GroupPayload),
    Editor(EditorPayload),
}

impl NodeKind {
    /// A short, user-facing label for what kind of node this is — used by
    /// generic UI (the node picker, placeholder widgets) that doesn't want
    /// to match on every variant itself.
    pub fn label(&self) -> &'static str {
        match self {
            NodeKind::Terminal(_) => "Terminal",
            NodeKind::Note(_) => "Note",
            NodeKind::Text(_) => "Text",
            NodeKind::FileTree(_) => "File Tree",
            NodeKind::Portal(_) => "Portal",
            NodeKind::Drawing(_) => "Drawing",
            NodeKind::Group(_) => "Group",
            NodeKind::Editor(_) => "Editor",
        }
    }
}

/// One object on the canvas: shared geometry/layering/state plus a
/// kind-specific payload. Replaces the old `SessionRecord`/`StickyNoteRecord`
/// split — every node, whatever its kind, is one of these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: Uuid,
    #[serde(default)]
    pub floor: FloorRef,
    pub position: (f64, f64),
    pub size: (f64, f64),
    /// Paint order among sibling nodes: higher paints later (on top).
    /// Not required to be contiguous — only relative order matters, so
    /// "bring to front" just needs to exceed the current maximum.
    #[serde(default)]
    pub z_order: i64,
    #[serde(default)]
    pub collapsed: bool,
    /// A locked node cannot be moved or resized by a drag. Persisted so the
    /// lock survives a restart.
    #[serde(default)]
    pub locked: bool,
    pub kind: NodeKind,
}

impl NodeRecord {
    /// Convenience for code that only cares about "is this a Terminal", the
    /// one kind with a live process — most of `app.rs`'s kind-specific logic
    /// needs exactly this question, not a full match.
    pub fn as_terminal(&self) -> Option<&TerminalPayload> {
        match &self.kind {
            NodeKind::Terminal(payload) => Some(payload),
            _ => None,
        }
    }

    pub fn as_terminal_mut(&mut self) -> Option<&mut TerminalPayload> {
        match &mut self.kind {
            NodeKind::Terminal(payload) => Some(payload),
            _ => None,
        }
    }

    /// Not yet called by any production code (only `as_note_mut` is, from
    /// note-editing), but it's the natural symmetric counterpart and
    /// Milestone 4's agent-readable notes API will want exactly this
    /// read-only accessor. Already exercised by this module's own tests.
    #[allow(dead_code)]
    pub fn as_note(&self) -> Option<&NotePayload> {
        match &self.kind {
            NodeKind::Note(payload) => Some(payload),
            _ => None,
        }
    }

    pub fn as_note_mut(&mut self) -> Option<&mut NotePayload> {
        match &mut self.kind {
            NodeKind::Note(payload) => Some(payload),
            _ => None,
        }
    }
}

/// A capability an edge can grant its target over its source (or vice versa
/// — direction is a future orchestration concern, not fixed by the model).
/// No permission *behavior* is implemented yet; these are representable now
/// so Milestone 3 doesn't need another migration to add them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EdgeCapability {
    SendMessages,
    ReadNote,
    WriteNote,
    ControlPortal,
    ShareContext,
}

/// A connection between two nodes by stable id. An empty `capabilities` set
/// is a plain visual-only connection — today's only kind, and still the
/// default for a link drawn on the canvas until something assigns it real
/// capabilities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeRecord {
    pub id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
    #[serde(default)]
    pub capabilities: std::collections::BTreeSet<EdgeCapability>,
}

impl EdgeRecord {
    pub fn visual(id: Uuid, source: Uuid, target: Uuid) -> EdgeRecord {
        EdgeRecord {
            id,
            source,
            target,
            capabilities: std::collections::BTreeSet::new(),
        }
    }

    /// Not yet read by any production code — nothing branches on an edge's
    /// capabilities until Milestone 3's orchestration/permissions work lands
    /// — but "an empty set means visual-only" is itself the Milestone 1
    /// requirement this predicate exists to make checkable, and it's already
    /// exercised by this module's tests.
    #[allow(dead_code)]
    pub fn is_visual_only(&self) -> bool {
        self.capabilities.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_ref_defaults_to_ground() {
        assert_eq!(FloorRef::default(), FloorRef::Ground);
    }

    #[test]
    fn node_kind_label_is_stable_per_variant() {
        assert_eq!(
            NodeKind::Terminal(TerminalPayload {
                name: "x".to_string(),
                cwd: PathBuf::from("/"),
                agent: Agent::Shell,
                claude_session_id: None,
                claude_account: None,
                never_launched: false,
                role_id: None,
                environment: EnvironmentKind::LocalPty,
            })
            .label(),
            "Terminal"
        );
        assert_eq!(NodeKind::Group(GroupPayload::default()).label(), "Group");
    }

    #[test]
    fn as_terminal_distinguishes_kinds() {
        let note = NodeRecord {
            id: Uuid::nil(),
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (1.0, 1.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Note(NotePayload {
                markdown: "hi".to_string(),
                color: "yellow".to_string(),
                view_mode: NoteViewMode::Preview,
                file: None,
            }),
        };
        assert!(note.as_terminal().is_none());
        assert!(note.as_note().is_some());
    }

    #[test]
    fn visual_edge_has_empty_capabilities() {
        let edge = EdgeRecord::visual(Uuid::nil(), Uuid::nil(), Uuid::nil());
        assert!(edge.is_visual_only());
    }

    #[test]
    fn edge_with_capabilities_is_not_visual_only() {
        let mut edge = EdgeRecord::visual(Uuid::nil(), Uuid::nil(), Uuid::nil());
        edge.capabilities.insert(EdgeCapability::SendMessages);
        assert!(!edge.is_visual_only());
    }

    #[test]
    fn node_record_round_trips_through_json() {
        let node = NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Floor(Uuid::new_v4()),
            position: (10.0, 20.0),
            size: (200.0, 100.0),
            z_order: 3,
            collapsed: true,
            locked: true,
            kind: NodeKind::Text(TextPayload {
                content: "hello".to_string(),
            }),
        };
        let json = serde_json::to_string(&node).unwrap();
        let back: NodeRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(node, back);
    }

    #[test]
    fn a_pre_milestone_6_placeholder_file_tree_loads_with_real_defaults() {
        let tree: FileTreePayload = serde_json::from_str(r#"{"root_label": "repo"}"#).unwrap();
        assert_eq!(tree.root_label, "repo");
        assert_eq!(tree.root, "");
        assert!(tree.expanded.is_empty());
        assert!(tree.respect_gitignore);
        assert!(tree.show_git_status);
        assert!(!tree.show_hidden);
    }

    #[test]
    fn file_tree_editor_and_file_backed_note_round_trip_through_json() {
        let tree = NodeKind::FileTree(FileTreePayload {
            root_label: String::new(),
            root: "src".to_string(),
            expanded: ["src/auth".to_string()].into_iter().collect(),
            selected: Some("src/auth/mod.rs".to_string()),
            show_hidden: true,
            respect_gitignore: false,
            show_git_status: true,
        });
        let editor = NodeKind::Editor(EditorPayload {
            path: "src/main.rs".to_string(),
            diff: Some(DiffScope::Staged),
        });
        let note = NodeKind::Note(NotePayload {
            markdown: "# Plan".to_string(),
            color: "yellow".to_string(),
            view_mode: NoteViewMode::Edit,
            file: Some(NoteFileBacking {
                path: "docs/plan.md".to_string(),
                revision: Some(FileRevision::of(b"# Plan")),
            }),
        });
        for kind in [tree, editor, note] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<NodeKind>(&json).unwrap(), kind);
        }
        // An internal note doesn't even mention `file`, keeping old readers happy.
        let internal = serde_json::to_string(&NotePayload {
            markdown: String::new(),
            color: "yellow".to_string(),
            view_mode: NoteViewMode::Edit,
            file: None,
        })
        .unwrap();
        assert!(!internal.contains("file"));
    }

    #[test]
    fn edge_record_round_trips_through_json() {
        let mut edge = EdgeRecord::visual(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        edge.capabilities.insert(EdgeCapability::ReadNote);
        edge.capabilities.insert(EdgeCapability::WriteNote);
        let json = serde_json::to_string(&edge).unwrap();
        let back: EdgeRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(edge, back);
    }
}
