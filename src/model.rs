//! The generalized canvas domain model: every object on the canvas is a
//! [`NodeRecord`] (a stable id plus shared position/size/layering/collapsed/
//! locked metadata, and a [`NodeKind`]-specific payload), and every
//! connection between two nodes is an [`EdgeRecord`] (stable source/target
//! ids plus a set of capabilities, empty meaning visual-only). Both are pure
//! data — no GTK, no runtime handles — persisted inside a `WorkspaceRecord`
//! (`store.rs`) and migrated by `migration.rs`.
//!
//! This module intentionally does not implement orchestration permission
//! behavior for edge capabilities, file-tree/portal/drawing functionality,
//! or group containment — those are later milestones. The payload types
//! here exist so the model is genuinely generic now, not because their
//! owning features are built.

use crate::agent::Agent;
use serde::{Deserialize, Serialize};
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
    /// `true` exactly while `claude_session_id` is pinned to an id that has
    /// never actually been used to launch Claude yet (freshly created,
    /// duplicated, or handed off to) — the signal `build_terminal_launch`
    /// uses to pick `--session-id` (create) over `--resume` (continue).
    /// Without this, a brand-new id with `claude_session_id.is_some()` but
    /// no real conversation behind it would launch with `--resume` and fail
    /// with "No conversation found with ID ...". `#[serde(default)]` so
    /// every terminal saved before this field existed loads as `false` —
    /// the safe assumption for a session that, by virtue of already being
    /// persisted, has necessarily been launched at least once before.
    #[serde(default)]
    pub claude_fresh: bool,
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

/// Placeholder for Milestone 7's file tree node: enough to exist, round-trip
/// through persistence, and render as a labeled placeholder. No filesystem
/// access, no `ProjectFilesystem`, no real tree.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FileTreePayload {
    pub root_label: String,
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
                claude_fresh: false,
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
    fn edge_record_round_trips_through_json() {
        let mut edge = EdgeRecord::visual(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        edge.capabilities.insert(EdgeCapability::ReadNote);
        edge.capabilities.insert(EdgeCapability::WriteNote);
        let json = serde_json::to_string(&edge).unwrap();
        let back: EdgeRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(edge, back);
    }
}
