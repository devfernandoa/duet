//! Turns the bytes of a `store.json` of any vintage into a current-shape
//! `Store`. `store.rs` owns on-disk mechanics (atomic save, where the file
//! lives); this module owns the question "what schema is this, and how do I
//! get from it to the one `App` understands" — the single place that answers
//! it, so a future schema change has exactly one spot to add a migration
//! step rather than a new shape-sniffing branch wherever `Store` is loaded.
//!
//! Schema versions so far:
//! - 1: the pre-canvas ratatui `tabs.json` shape (`{"tabs": [...]}}`)
//! - 2: the pre-workspace single-canvas shape (top-level `sessions`/`notes`/
//!   `links`/`canvas`, no `workspaces` key)
//! - 3: the first multi-workspace shape: `workspaces`/`active_workspace`/
//!   `custom_roles`, where each workspace holds `sessions`/`notes`/`links`.
//!   Files written before the `schema_version` field existed have this shape
//!   but no `"schema_version"` key at all; they are treated as 3, since 3 is
//!   the only version this shape has ever had.
//! - 4: each workspace holds generic `nodes`/`edges`
//!   (`model::NodeRecord`/`model::EdgeRecord`) instead of the kind-specific
//!   `sessions`/`notes`/`links` — Milestone 1's generalized canvas model. A
//!   schema-3 session becomes a `Terminal` node, a sticky note becomes a
//!   `Note` node (its plain text becoming Markdown source unchanged — plain
//!   text is already valid Markdown), and a link becomes a visual-only
//!   (empty-capability) edge.
//! - 5: purely additive — Milestone 2's runtime-survival work adds
//!   `TerminalPayload::environment` and `WorkspaceRecord::{environment,
//!   color, icon, created_at, last_opened}`. Every new field is
//!   `#[serde(default)]`, so a schema-4 file (which has none of them)
//!   deserializes directly as the current `Store` with no dedicated
//!   conversion step — unlike schema 3 -> 4, the on-disk *shape* of a node
//!   didn't change, only which optional fields a `WorkspaceRecord`/
//!   `TerminalPayload` may carry.
//! - 6: purely additive again — Milestone 6's project filesystem
//!   work. `FileTreePayload` gains real view state (`root`, `expanded`,
//!   `selected`, `show_hidden`, `respect_gitignore`, `show_git_status`),
//!   `NotePayload` gains an optional `file` backing, and `NodeKind` gains an
//!   `Editor` variant. Every new field is `#[serde(default)]` (a schema-5
//!   placeholder FileTree, which only had `root_label`, loads as a real tree
//!   of the project root; every schema-5 note loads as an internal note), so
//!   a schema-5 file deserializes directly with no conversion step. The
//!   version still moves so a file that *uses* the new shape is never
//!   silently half-read by an older binary: that binary sees version 6 and
//!   takes the future-schema backup path below instead.
//! - 7: Milestone 8's browser portals. `PortalPayload` (until now
//!   a placeholder holding only `url`) gains `name`, a required `profile`
//!   (`model::PortalProfile`, the portal's isolated browser-storage
//!   identity) and `allow_scripts`. `name`/`allow_scripts` default through
//!   serde, but a profile id must be *stable* — a random serde default
//!   would hand the same portal a different cookie jar on every load until
//!   the next save — so [`migrate_v6_portals`] derives one explicitly and
//!   deterministically from the node id (UUIDv5) for every pre-7 portal.
//!   Every other node is untouched.
//! - 8 (current): Milestone 7.5's real Drawing and Group nodes. A
//!   `DrawingPayload` gains `strokes` (vector, normalized points) and a
//!   `GroupPayload` gains `color`; both `#[serde(default)]`, so a schema-7
//!   placeholder Drawing loads as an empty drawing and a placeholder Group
//!   (with its possibly-empty `label`) as a blue section titled "Group" —
//!   no conversion step. The version moves so an older binary, which would
//!   silently drop the strokes and colors on its next save, refuses the
//!   file instead (the future-schema backup path).
//!
//! A `schema_version` *greater* than [`CURRENT_SCHEMA_VERSION`] means the
//! file was written by a newer duet. Rather than guess at a shape it has
//! never seen, this module refuses to touch it: the original file is backed
//! up untouched and nothing is migrated, so upgrading duet (not editing the
//! file by hand) is the only way to recover it.

use crate::agent::Agent;
use crate::model::{
    EdgeRecord, EnvironmentKind, FloorRef, NodeKind, NodeRecord, NotePayload, NoteViewMode,
    TerminalPayload,
};
use crate::role::Role;
use crate::store::{CanvasRecord, Store, WorkspaceRecord};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The schema version this binary reads and writes. Bump this and add a
/// migration step below whenever `Store`'s on-disk shape changes.
pub const CURRENT_SCHEMA_VERSION: u32 = 8;

/// Plain-function form of [`CURRENT_SCHEMA_VERSION`], for serde's
/// `#[serde(default = "...")]` attribute on `Store::schema_version` (which
/// needs a callable path, not a const).
pub fn current_schema_version_default() -> u32 {
    CURRENT_SCHEMA_VERSION
}

/// The pre-canvas on-disk shape. Kept only to migrate old `tabs.json` files
/// written by the ratatui version of duet.
#[derive(Debug, Deserialize)]
struct LegacyTabRecord {
    name: String,
    cwd: PathBuf,
    agent: Agent,
    claude_session_id: Option<Uuid>,
    claude_account: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyStore {
    tabs: Vec<LegacyTabRecord>,
}

/// Schema 3's per-session shape: a `Terminal` node's data plus its own
/// position/size, before those moved onto the generic `NodeRecord`.
#[derive(Debug, Deserialize)]
struct V3SessionRecord {
    id: Uuid,
    name: String,
    cwd: PathBuf,
    agent: Agent,
    claude_session_id: Option<Uuid>,
    claude_account: Option<String>,
    #[serde(default)]
    role_id: Option<Uuid>,
    position: (f64, f64),
    size: (f64, f64),
}

/// Schema 3's per-note shape: plain text, not yet Markdown-aware.
#[derive(Debug, Deserialize)]
struct V3StickyNoteRecord {
    id: Uuid,
    text: String,
    position: (f64, f64),
    size: (f64, f64),
    color: String,
}

/// Schema 3's link shape: a bare, capability-less connection.
#[derive(Debug, Clone, Copy, Deserialize)]
struct V3LinkRecord {
    source: Uuid,
    target: Uuid,
}

#[derive(Debug, Deserialize)]
struct V3WorkspaceRecord {
    id: Uuid,
    name: String,
    root_dir: PathBuf,
    #[serde(default)]
    sessions: Vec<V3SessionRecord>,
    #[serde(default)]
    notes: Vec<V3StickyNoteRecord>,
    #[serde(default)]
    links: Vec<V3LinkRecord>,
    #[serde(default)]
    canvas: CanvasRecord,
}

#[derive(Debug, Deserialize)]
struct V3Store {
    #[serde(default)]
    workspaces: Vec<V3WorkspaceRecord>,
    #[serde(default)]
    active_workspace: Option<Uuid>,
    #[serde(default)]
    custom_roles: Vec<Role>,
}

/// The pre-workspace on-disk shape: a single flat canvas, no `workspaces`
/// key. Kept only to migrate files written before workspaces existed.
#[derive(Debug, Deserialize)]
struct PreWorkspaceStore {
    #[serde(default)]
    sessions: Vec<V3SessionRecord>,
    #[serde(default)]
    notes: Vec<V3StickyNoteRecord>,
    #[serde(default)]
    links: Vec<V3LinkRecord>,
    #[serde(default)]
    canvas: CanvasRecord,
}

const GRID_COLUMNS: f64 = 3.0;
const GRID_CELL_WIDTH: f64 = 520.0;
const GRID_CELL_HEIGHT: f64 = 360.0;
const DEFAULT_NODE_SIZE: (f64, f64) = (480.0, 320.0);

/// Name and root directory given to a workspace created by migrating an
/// older single-canvas store, so existing users don't lose their canvas.
fn default_workspace_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

fn convert_session(session: V3SessionRecord) -> NodeRecord {
    NodeRecord {
        id: session.id,
        floor: FloorRef::Ground,
        position: session.position,
        size: session.size,
        z_order: 0,
        collapsed: false,
        locked: false,
        kind: NodeKind::Terminal(TerminalPayload {
            name: session.name,
            cwd: session.cwd,
            agent: session.agent,
            claude_session_id: session.claude_session_id,
            claude_account: session.claude_account,
            // A migrated session has necessarily run before.
            never_launched: false,
            role_id: session.role_id,
            environment: EnvironmentKind::LocalPty,
        }),
    }
}

/// A sticky note's plain text becomes Markdown source unchanged — arbitrary
/// plain text is already valid Markdown (no special syntax to escape), so
/// this is a lossless, content-preserving rename rather than a conversion.
/// Opens in Preview, matching every other migrated note having no "mode" of
/// its own to carry forward.
fn convert_note(note: V3StickyNoteRecord) -> NodeRecord {
    NodeRecord {
        id: note.id,
        floor: FloorRef::Ground,
        position: note.position,
        size: note.size,
        z_order: 0,
        collapsed: false,
        locked: false,
        kind: NodeKind::Note(NotePayload {
            markdown: note.text,
            color: note.color,
            view_mode: NoteViewMode::Preview,
            file: None,
        }),
    }
}

/// A schema-3 link carried no capabilities at all — it was purely visual.
/// `EdgeRecord::visual` is exactly that: an empty capability set.
fn convert_link(link: V3LinkRecord) -> EdgeRecord {
    EdgeRecord::visual(Uuid::new_v4(), link.source, link.target)
}

fn convert_workspace(workspace: V3WorkspaceRecord) -> WorkspaceRecord {
    let mut nodes: Vec<NodeRecord> = workspace
        .sessions
        .into_iter()
        .map(convert_session)
        .collect();
    nodes.extend(workspace.notes.into_iter().map(convert_note));
    let edges = workspace.links.into_iter().map(convert_link).collect();
    WorkspaceRecord {
        id: workspace.id,
        name: workspace.name,
        root_dir: workspace.root_dir,
        nodes,
        edges,
        canvas: workspace.canvas,
        environment: EnvironmentKind::LocalPty,
        color: None,
        icon: None,
        // Unknown, not fabricated: a pre-Milestone-2 file never recorded
        // when a workspace was created or last opened.
        created_at: 0,
        last_opened: 0,
    }
}

fn migrate_v3_to_v4(v3: V3Store) -> Store {
    let workspaces = v3.workspaces.into_iter().map(convert_workspace).collect();
    Store::new(workspaces, v3.active_workspace, v3.custom_roles)
}

/// Wraps one flat canvas's worth of schema-3 records into a current-shape
/// `Store` containing a single "Default" workspace, used by both the
/// schema-1 and schema-2 migration paths below (both predate workspaces
/// existing at all, so both land here before going through the same
/// session/note/link -> node/edge conversion schema 3 -> 4 uses).
fn wrap_single_workspace(
    sessions: Vec<V3SessionRecord>,
    notes: Vec<V3StickyNoteRecord>,
    links: Vec<V3LinkRecord>,
    canvas: CanvasRecord,
) -> Store {
    let id = Uuid::new_v4();
    let workspace = convert_workspace(V3WorkspaceRecord {
        id,
        name: "Default".to_string(),
        root_dir: default_workspace_root(),
        sessions,
        notes,
        links,
        canvas,
    });
    Store::new(vec![workspace], Some(id), Vec::new())
}

fn migrate_legacy(legacy: LegacyStore) -> Store {
    let sessions = legacy
        .tabs
        .into_iter()
        .enumerate()
        .map(|(index, tab)| {
            let column = (index as f64) % GRID_COLUMNS;
            let row = (index as f64 / GRID_COLUMNS).floor();
            V3SessionRecord {
                id: Uuid::new_v4(),
                name: tab.name,
                cwd: tab.cwd,
                agent: tab.agent,
                claude_session_id: tab.claude_session_id,
                claude_account: tab.claude_account,
                role_id: None,
                position: (column * GRID_CELL_WIDTH, row * GRID_CELL_HEIGHT),
                size: DEFAULT_NODE_SIZE,
            }
        })
        .collect();
    wrap_single_workspace(sessions, Vec::new(), Vec::new(), CanvasRecord::default())
}

/// Copies the unreadable file to `path` with its extension replaced by
/// `backup_suffix`, then returns an empty `Store` paired with a warning —
/// the shared "don't lose it, but don't use it either" response for any file
/// this module can't safely interpret.
fn fail_safe(path: &Path, backup_suffix: &str, message: String) -> (Store, Option<String>) {
    let backup = path.with_extension(backup_suffix);
    let backup_note = match std::fs::copy(path, &backup) {
        Ok(_) => format!(" A backup was saved to {}.", backup.display()),
        Err(_) => String::new(),
    };
    (Store::default(), Some(format!("{message}{backup_note}")))
}

fn backup_corrupt_file(path: &Path, reason: &str) -> (Store, Option<String>) {
    fail_safe(
        path,
        "corrupt.json",
        format!("Couldn't read the saved workspace ({reason})."),
    )
}

/// The fail-safe path for a `schema_version` newer than this binary knows
/// about (see the module doc comment). Deliberately a different backup
/// suffix than `backup_corrupt_file`'s — the file isn't corrupt, it's just
/// from the future, and a user who goes looking for a backup should be able
/// to tell the two apart.
fn backup_future_schema_file(path: &Path, found_version: u32) -> (Store, Option<String>) {
    fail_safe(
        path,
        "future-schema.json",
        format!(
            "This workspace file was saved by a newer version of duet (schema version \
             {found_version}; this build understands up to {CURRENT_SCHEMA_VERSION}). Nothing \
             was changed — update duet to open it."
        ),
    )
}

/// The deterministic profile id a pre-schema-7 portal is given: the same
/// node always maps to the same profile, so loading an old store twice
/// (say, a backup) never splits one portal's browser data in two.
pub fn migrated_portal_profile_id(node_id: Uuid) -> Uuid {
    Uuid::new_v5(&node_id, b"duet-portal-profile")
}

/// Schema 6 -> 7: gives every `Portal` node (in every workspace) the fields
/// a schema-7 portal requires — a `name` and an isolated, persistent
/// `profile` with a deterministic id — leaving any field already present,
/// and every non-portal node, exactly as it was. Works on the raw JSON so
/// the current `PortalPayload` can require `profile` without a random
/// serde default.
fn migrate_v6_portals(store: &mut serde_json::Value) {
    let Some(workspaces) = store
        .get_mut("workspaces")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for workspace in workspaces {
        let Some(nodes) = workspace
            .get_mut("nodes")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for node in nodes {
            let node_id = node
                .get("id")
                .and_then(serde_json::Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok());
            let Some(kind) = node
                .get_mut("kind")
                .and_then(serde_json::Value::as_object_mut)
            else {
                continue;
            };
            if kind.get("kind").and_then(serde_json::Value::as_str) != Some("Portal") {
                continue;
            }
            kind.entry("name")
                .or_insert_with(|| crate::model::DEFAULT_PORTAL_NAME.into());
            kind.entry("url").or_insert_with(|| "".into());
            if !kind.contains_key("profile") {
                let profile_id = node_id
                    .map(migrated_portal_profile_id)
                    .unwrap_or_else(Uuid::new_v4);
                kind.insert(
                    "profile".to_string(),
                    serde_json::json!({"id": profile_id, "storage": "persistent"}),
                );
            }
        }
    }
}

fn declared_schema_version(obj: &serde_json::Map<String, serde_json::Value>) -> Option<u32> {
    obj.get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
}

/// Parses `contents` (the raw bytes of a store file) and migrates it to the
/// current `Store` shape, whatever vintage it turns out to be. Called by
/// `Store::load_with_warning`, which handles the file-level I/O (missing
/// file, read errors) around this.
pub fn load(path: &Path, contents: &str) -> (Store, Option<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(contents) else {
        return backup_corrupt_file(path, "Invalid JSON");
    };

    // Schema 1: the ratatui `tabs.json` shape, checked first since it has no
    // `workspaces`/`sessions` keys to otherwise distinguish it.
    if value.get("tabs").is_some() {
        return match serde_json::from_value::<LegacyStore>(value) {
            Ok(legacy) => (migrate_legacy(legacy), None),
            Err(error) => backup_corrupt_file(path, &error.to_string()),
        };
    }

    let Some(obj) = value.as_object() else {
        return backup_corrupt_file(path, "JSON is not an object");
    };

    // Schemas 3 through 6 all have a top-level "workspaces" key; a missing
    // `schema_version` has only ever meant 3 (the shape before the field
    // existed), so that's the default rather than assuming "current".
    if obj.contains_key("workspaces") {
        let version = declared_schema_version(obj).unwrap_or(3);
        if version > CURRENT_SCHEMA_VERSION {
            return backup_future_schema_file(path, version);
        }
        if version >= 4 {
            // Schemas 4 through 8 share the same nodes/edges shape; 5, 6 and
            // 8 only add new fields/variants, every field `#[serde(default)]`,
            // so such a file deserializes directly as the current `Store`
            // with no dedicated conversion step. 7 additionally needs every
            // portal's stable profile id filled in first.
            let mut value = value;
            if version < 7 {
                migrate_v6_portals(&mut value);
            }
            return match serde_json::from_value::<Store>(value) {
                Ok(store) => (store, None),
                Err(error) => backup_corrupt_file(path, &error.to_string()),
            };
        }
        // Only schema 3 can appear here today (version <= 3, since anything
        // greater returned above and 4+ is handled above) — migrate
        // sessions/notes/links into nodes/edges.
        return match serde_json::from_value::<V3Store>(value) {
            Ok(v3) => (migrate_v3_to_v4(v3), None),
            Err(error) => backup_corrupt_file(path, &error.to_string()),
        };
    }

    // Schema 2: the pre-workspace single-canvas shape.
    if obj.contains_key("sessions")
        || obj.contains_key("notes")
        || obj.contains_key("links")
        || obj.contains_key("canvas")
    {
        return match serde_json::from_value::<PreWorkspaceStore>(value) {
            Ok(pre) => (
                wrap_single_workspace(pre.sessions, pre.notes, pre.links, pre.canvas),
                None,
            ),
            Err(error) => backup_corrupt_file(path, &error.to_string()),
        };
    }

    // A valid JSON object with none of the recognized keys and no "tabs" —
    // ambiguous/truncated data, not a shape this module has ever produced.
    backup_corrupt_file(path, "unrecognized JSON structure")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeKind;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn current_v4_shape_loads_directly_with_no_migration() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r#"{
                "schema_version": 4,
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000000",
                    "name": "web",
                    "root_dir": "/home/fernando",
                    "nodes": [],
                    "edges": [],
                    "canvas": {"zoom": 1.0, "pan": [0.0, 0.0]}
                }],
                "active_workspace": "00000000-0000-0000-0000-000000000000"
            }"#,
        );
        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        assert_eq!(store.schema_version, 4);
        assert_eq!(store.workspaces.len(), 1);
    }

    /// A schema-4 file (Milestone 1, pre-Milestone-2) has none of
    /// `WorkspaceRecord`'s new `environment`/`color`/`icon`/`created_at`/
    /// `last_opened` keys and no `TerminalPayload::environment` key either.
    /// Confirms it loads directly (no dedicated v4->v5 struct needed) with
    /// every new field defaulting sensibly rather than failing to parse.
    #[test]
    fn v4_shape_loads_with_new_milestone_2_fields_defaulted() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r#"{
                "schema_version": 4,
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000000",
                    "name": "web",
                    "root_dir": "/home/fernando",
                    "nodes": [{
                        "id": "00000000-0000-0000-0000-000000000001",
                        "position": [0.0, 0.0],
                        "size": [480.0, 320.0],
                        "kind": {
                            "kind": "Terminal",
                            "name": "a",
                            "cwd": "/tmp",
                            "agent": "Codex",
                            "claude_session_id": null,
                            "claude_account": null
                        }
                    }],
                    "edges": [],
                    "canvas": {"zoom": 1.0, "pan": [0.0, 0.0]}
                }],
                "active_workspace": "00000000-0000-0000-0000-000000000000"
            }"#,
        );
        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        assert_eq!(store.schema_version, 4);
        let workspace = &store.workspaces[0];
        assert_eq!(workspace.environment, EnvironmentKind::LocalPty);
        assert_eq!(workspace.color, None);
        assert_eq!(workspace.icon, None);
        assert_eq!(workspace.created_at, 0);
        assert_eq!(workspace.last_opened, 0);
        assert_eq!(
            workspace.nodes[0].as_terminal().unwrap().environment,
            EnvironmentKind::LocalPty
        );
    }

    /// A schema-5 file (Milestone 5) with a placeholder FileTree (only
    /// `root_label`) and an internal note loads directly: the FileTree gets
    /// real default view state, the note stays internal, and nothing else
    /// about either node is lost. Saving it again stamps schema 6.
    #[test]
    fn v5_shape_loads_with_milestone_6_defaults_and_saves_as_v6() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r##"{
                "schema_version": 5,
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000000",
                    "name": "web",
                    "root_dir": "/home/fernando/web",
                    "nodes": [
                        {
                            "id": "00000000-0000-0000-0000-000000000001",
                            "position": [10.0, 20.0],
                            "size": [220.0, 160.0],
                            "kind": {"kind": "FileTree", "root_label": "web"}
                        },
                        {
                            "id": "00000000-0000-0000-0000-000000000002",
                            "position": [0.0, 0.0],
                            "size": [220.0, 160.0],
                            "kind": {
                                "kind": "Note",
                                "markdown": "# Plan",
                                "color": "yellow",
                                "view_mode": "Edit"
                            }
                        }
                    ],
                    "edges": [],
                    "canvas": {"zoom": 1.0, "pan": [0.0, 0.0]}
                }],
                "active_workspace": "00000000-0000-0000-0000-000000000000"
            }"##,
        );
        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        let nodes = &store.workspaces[0].nodes;
        let NodeKind::FileTree(tree) = &nodes[0].kind else {
            panic!("expected a FileTree");
        };
        assert_eq!(tree.root_label, "web");
        assert_eq!(tree.root, "");
        assert!(tree.respect_gitignore && tree.show_git_status && !tree.show_hidden);
        assert_eq!(nodes[0].position, (10.0, 20.0));
        let note = nodes[1].as_note().unwrap();
        assert_eq!(note.markdown, "# Plan");
        assert_eq!(note.file, None);

        let resaved = Store::new(store.workspaces.clone(), store.active_workspace, Vec::new());
        resaved.save(&path).unwrap();
        let (reloaded, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        assert_eq!(reloaded.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(reloaded.workspaces[0].nodes, store.workspaces[0].nodes);
    }

    #[test]
    fn v6_placeholder_portals_migrate_to_named_portals_with_stable_profiles() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        let contents = r##"{
            "schema_version": 6,
            "workspaces": [{
                "id": "00000000-0000-0000-0000-000000000000",
                "name": "web",
                "root_dir": "/home/fernando/web",
                "nodes": [
                    {
                        "id": "00000000-0000-0000-0000-00000000000a",
                        "position": [5.0, 6.0],
                        "size": [220.0, 160.0],
                        "kind": {"kind": "Portal", "url": "http://localhost:3000"}
                    },
                    {
                        "id": "00000000-0000-0000-0000-00000000000b",
                        "position": [0.0, 0.0],
                        "size": [220.0, 160.0],
                        "kind": {"kind": "Portal", "url": ""}
                    },
                    {
                        "id": "00000000-0000-0000-0000-00000000000c",
                        "position": [0.0, 0.0],
                        "size": [220.0, 160.0],
                        "kind": {"kind": "Text", "content": "untouched"}
                    }
                ],
                "edges": [],
                "canvas": {"zoom": 1.0, "pan": [0.0, 0.0]}
            }],
            "active_workspace": "00000000-0000-0000-0000-000000000000"
        }"##;
        write(&path, contents);
        let (store, warning) = load(&path, contents);
        assert!(warning.is_none(), "{warning:?}");
        let nodes = &store.workspaces[0].nodes;
        let first = nodes[0].as_portal().unwrap();
        assert_eq!(first.name, "Portal");
        assert_eq!(first.url, "http://localhost:3000");
        assert_eq!(first.profile.id, migrated_portal_profile_id(nodes[0].id));
        assert_eq!(
            first.profile.storage,
            crate::model::PortalStorage::Persistent
        );
        assert!(!first.allow_scripts);
        assert_eq!(nodes[0].position, (5.0, 6.0));
        // Each portal gets its own profile: isolation by default.
        let second = nodes[1].as_portal().unwrap();
        assert_ne!(first.profile.id, second.profile.id);
        assert!(matches!(&nodes[2].kind, NodeKind::Text(t) if t.content == "untouched"));

        // Deterministic: loading the same old file again gives the same ids.
        let (again, _) = load(&path, contents);
        assert_eq!(again.workspaces[0].nodes, store.workspaces[0].nodes);

        // Saved as 7 and reloaded unchanged (no second migration).
        let resaved = Store::new(store.workspaces.clone(), store.active_workspace, Vec::new());
        resaved.save(&path).unwrap();
        let (reloaded, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        assert_eq!(reloaded.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(reloaded.workspaces[0].nodes, store.workspaces[0].nodes);
    }

    /// Schema 7 had Drawing and Group only as placeholders: a Drawing with
    /// no data at all and a Group with just a (often empty) label. Both
    /// load as real nodes with their geometry and layering intact, and
    /// round-trip through a schema-8 save.
    #[test]
    fn v7_placeholder_groups_and_drawings_load_as_real_nodes() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        let contents = r##"{
            "schema_version": 7,
            "workspaces": [{
                "id": "00000000-0000-0000-0000-000000000000",
                "name": "web",
                "root_dir": "/home/fernando/web",
                "nodes": [
                    {
                        "id": "00000000-0000-0000-0000-00000000000a",
                        "position": [10.0, 20.0],
                        "size": [600.0, 400.0],
                        "z_order": 4,
                        "locked": true,
                        "kind": {"kind": "Group", "label": ""}
                    },
                    {
                        "id": "00000000-0000-0000-0000-00000000000b",
                        "position": [0.0, 0.0],
                        "size": [220.0, 160.0],
                        "kind": {"kind": "Group", "label": "Backend"}
                    },
                    {
                        "id": "00000000-0000-0000-0000-00000000000c",
                        "position": [30.0, 40.0],
                        "size": [300.0, 200.0],
                        "kind": {"kind": "Drawing"}
                    }
                ],
                "edges": [],
                "canvas": {"zoom": 1.0, "pan": [0.0, 0.0]}
            }],
            "active_workspace": "00000000-0000-0000-0000-000000000000"
        }"##;
        write(&path, contents);
        let (store, warning) = load(&path, contents);
        assert!(warning.is_none(), "{warning:?}");
        let nodes = &store.workspaces[0].nodes;
        let NodeKind::Group(first) = &nodes[0].kind else {
            panic!("expected a group")
        };
        assert_eq!(first.title(), "Group");
        assert_eq!(first.color_name(), "blue");
        assert_eq!(nodes[0].position, (10.0, 20.0));
        assert_eq!(nodes[0].size, (600.0, 400.0));
        assert_eq!(nodes[0].z_order, 4);
        assert!(nodes[0].locked);
        assert!(matches!(&nodes[1].kind, NodeKind::Group(g) if g.title() == "Backend"));
        assert!(matches!(&nodes[2].kind, NodeKind::Drawing(d) if d.strokes.is_empty()));

        let resaved = Store::new(store.workspaces.clone(), store.active_workspace, Vec::new());
        resaved.save(&path).unwrap();
        let (reloaded, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        assert_eq!(reloaded.schema_version, 8);
        assert_eq!(reloaded.workspaces[0].nodes, store.workspaces[0].nodes);
    }

    /// A schema-3 file saved by the pre-Milestone-1 binary has no
    /// `"schema_version"` key at all (it predates the field existing on that
    /// shape). It must still be recognized as 3 and migrated, not read as
    /// though it already had `nodes`/`edges`.
    #[test]
    fn v3_shape_without_explicit_schema_version_migrates_to_current() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r#"{
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000000",
                    "name": "web",
                    "root_dir": "/home/fernando",
                    "sessions": [],
                    "notes": [],
                    "links": [],
                    "canvas": {"zoom": 1.0, "pan": [0.0, 0.0]}
                }],
                "active_workspace": "00000000-0000-0000-0000-000000000000"
            }"#,
        );
        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        assert_eq!(store.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(store.workspaces.len(), 1);
        assert!(store.workspaces[0].nodes.is_empty());
        assert!(store.workspaces[0].edges.is_empty());
    }

    #[test]
    fn v3_shape_with_explicit_schema_version_three_migrates_to_current() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r#"{
                "schema_version": 3,
                "workspaces": [],
                "active_workspace": null
            }"#,
        );
        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        assert_eq!(store.schema_version, CURRENT_SCHEMA_VERSION);
    }

    /// The core requirement: a store from a newer duet must not be silently
    /// reset or misread. The returned `Store` is empty (fail-safe default),
    /// but the original bytes are recoverable from the backup file untouched
    /// — nothing about the user's real data was discarded.
    #[test]
    fn future_schema_version_fails_safely_without_discarding_data() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        let original = r#"{
            "schema_version": 999,
            "workspaces": [{
                "id": "00000000-0000-0000-0000-000000000000",
                "name": "from-the-future",
                "root_dir": "/home/fernando",
                "nodes": [],
                "edges": [],
                "canvas": {"zoom": 1.0, "pan": [0.0, 0.0]}
            }],
            "active_workspace": "00000000-0000-0000-0000-000000000000"
        }"#;
        write(&path, original);

        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());

        assert!(store.workspaces.is_empty());
        let warning = warning.expect("a future schema version must produce a warning");
        assert!(warning.contains("999"));
        assert!(warning.contains(&CURRENT_SCHEMA_VERSION.to_string()));

        let backup = path.with_extension("future-schema.json");
        assert!(backup.exists(), "expected a backup of the unreadable file");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), original);
    }

    #[test]
    fn load_corrupt_file_returns_empty_store_and_backs_it_up() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(&path, "{not valid json");
        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(store.workspaces.is_empty());
        assert!(warning.is_some());
        assert!(path.with_extension("corrupt.json").exists());
    }

    #[test]
    fn ambiguous_json_object_is_treated_as_corrupt() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(&path, "{}");
        let (store, _) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(store.workspaces.is_empty());
        assert!(path.with_extension("corrupt.json").exists());
    }

    #[test]
    fn loading_old_tabs_shape_migrates_into_terminal_nodes_at_current_schema() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r#"{"tabs": [
                {"name": "a", "cwd": "/tmp", "agent": "Codex", "claude_session_id": null, "claude_account": null, "codex_used": true},
                {"name": "b", "cwd": "/tmp", "agent": "Codex", "claude_session_id": null, "claude_account": null, "codex_used": true}
            ]}"#,
        );
        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        assert_eq!(store.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(store.workspaces.len(), 1);
        let workspace = &store.workspaces[0];
        assert_eq!(store.active_workspace, Some(workspace.id));
        assert_eq!(workspace.nodes.len(), 2);
        assert_ne!(workspace.nodes[0].id, workspace.nodes[1].id);
        assert_eq!(workspace.nodes[0].position, (0.0, 0.0));
        assert_eq!(workspace.nodes[1].position, (520.0, 0.0));
        assert_eq!(workspace.canvas.zoom, 1.0);
        assert_eq!(
            workspace.nodes[0].as_terminal().unwrap().agent,
            Agent::Codex
        );
    }

    /// The exact scenario schema 2 -> 4 migration exists for: a user
    /// upgrading from the single-canvas (pre-workspace) version must not
    /// lose their canvas — it becomes one workspace of nodes, not an empty
    /// store.
    #[test]
    fn loading_pre_workspace_single_canvas_shape_migrates_into_terminal_node() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r#"{
                "sessions": [{"id": "00000000-0000-0000-0000-000000000000", "name": "web", "cwd": "/tmp", "agent": "Codex", "claude_session_id": null, "claude_account": null, "position": [10.0, 20.0], "size": [480.0, 320.0]}],
                "notes": [],
                "links": [],
                "canvas": {"zoom": 2.0, "pan": [5.0, 6.0]}
            }"#,
        );
        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        assert_eq!(store.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(store.workspaces.len(), 1);
        let workspace = &store.workspaces[0];
        assert_eq!(store.active_workspace, Some(workspace.id));
        assert_eq!(workspace.nodes.len(), 1);
        assert_eq!(workspace.nodes[0].as_terminal().unwrap().name, "web");
        assert_eq!(workspace.canvas.zoom, 2.0);
    }

    /// Milestone 3 added three `Agent` variants alongside the original
    /// `Claude`/`Codex` unit variants. A workspace saved by an older duet
    /// binary has `"agent": "Claude"`/`"agent": "Codex"` as bare JSON
    /// strings (serde's default unit-variant representation) — confirms
    /// that literal shape still deserializes and migrates, i.e. existing
    /// saved sessions are preserved.
    #[test]
    fn old_bare_string_agent_values_still_deserialize_and_migrate() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r#"{
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000000",
                    "name": "web",
                    "root_dir": "/home/fernando",
                    "sessions": [
                        {"id": "00000000-0000-0000-0000-000000000001", "name": "a", "cwd": "/tmp", "agent": "Claude", "claude_session_id": null, "claude_account": null, "position": [0.0, 0.0], "size": [480.0, 320.0]},
                        {"id": "00000000-0000-0000-0000-000000000002", "name": "b", "cwd": "/tmp", "agent": "Codex", "claude_session_id": null, "claude_account": null, "position": [0.0, 0.0], "size": [480.0, 320.0]}
                    ],
                    "notes": [],
                    "links": [],
                    "canvas": {"zoom": 1.0, "pan": [0.0, 0.0]}
                }],
                "active_workspace": "00000000-0000-0000-0000-000000000000"
            }"#,
        );
        let (store, _) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert_eq!(
            store.workspaces[0].nodes[0].as_terminal().unwrap().agent,
            Agent::Claude
        );
        assert_eq!(
            store.workspaces[0].nodes[1].as_terminal().unwrap().agent,
            Agent::Codex
        );
    }

    /// A store saved before milestone 4 has no `"custom_roles"` key and no
    /// session record has a `"role_id"` key at all — confirms both default
    /// to empty/`None` rather than failing to load or migrate.
    #[test]
    fn store_without_roles_still_migrates_with_sessions_unassigned() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r#"{
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000000",
                    "name": "web",
                    "root_dir": "/home/fernando",
                    "sessions": [
                        {"id": "00000000-0000-0000-0000-000000000001", "name": "a", "cwd": "/tmp", "agent": "Claude", "claude_session_id": null, "claude_account": null, "position": [0.0, 0.0], "size": [480.0, 320.0]}
                    ],
                    "notes": [],
                    "links": [],
                    "canvas": {"zoom": 1.0, "pan": [0.0, 0.0]}
                }],
                "active_workspace": "00000000-0000-0000-0000-000000000000"
            }"#,
        );
        let (store, _) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert_eq!(
            store.workspaces[0].nodes[0].as_terminal().unwrap().role_id,
            None
        );
        assert!(store.custom_roles.is_empty());
    }

    /// The full Milestone 1 lossless-migration contract in one place: a
    /// schema-3 workspace with a session, a sticky note, and a link between
    /// two sessions all survive as the equivalent node/edge, with every
    /// field preserved and the link becoming a visual-only (empty
    /// capability set) edge.
    #[test]
    fn full_v3_workspace_migrates_losslessly() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        write(
            &path,
            r##"{
                "workspaces": [{
                    "id": "00000000-0000-0000-0000-000000000000",
                    "name": "web",
                    "root_dir": "/home/fernando",
                    "sessions": [
                        {"id": "00000000-0000-0000-0000-000000000001", "name": "backend", "cwd": "/srv/app", "agent": "Claude", "claude_session_id": "00000000-0000-0000-0000-00000000000a", "claude_account": "work", "role_id": "00000000-0000-0000-0000-00000000000b", "position": [1.0, 2.0], "size": [480.0, 320.0]},
                        {"id": "00000000-0000-0000-0000-000000000002", "name": "frontend", "cwd": "/srv/app/ui", "agent": "Codex", "claude_session_id": null, "claude_account": null, "position": [500.0, 2.0], "size": [480.0, 320.0]}
                    ],
                    "notes": [
                        {"id": "00000000-0000-0000-0000-000000000003", "text": "# TODO\n- [ ] ship it", "position": [0.0, 400.0], "size": [220.0, 160.0], "color": "yellow"}
                    ],
                    "links": [
                        {"source": "00000000-0000-0000-0000-000000000001", "target": "00000000-0000-0000-0000-000000000002"}
                    ],
                    "canvas": {"zoom": 1.25, "pan": [10.0, 20.0]}
                }],
                "active_workspace": "00000000-0000-0000-0000-000000000000"
            }"##,
        );
        let (store, warning) = load(&path, &std::fs::read_to_string(&path).unwrap());
        assert!(warning.is_none());
        let workspace = &store.workspaces[0];
        assert_eq!(workspace.nodes.len(), 3);
        assert_eq!(workspace.edges.len(), 1);

        let backend = workspace
            .nodes
            .iter()
            .find(|n| n.id == Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
            .unwrap();
        let backend_terminal = backend.as_terminal().unwrap();
        assert_eq!(backend_terminal.name, "backend");
        assert_eq!(backend_terminal.cwd, PathBuf::from("/srv/app"));
        assert_eq!(backend_terminal.agent, Agent::Claude);
        assert_eq!(backend_terminal.claude_account.as_deref(), Some("work"));
        assert!(backend_terminal.role_id.is_some());
        assert_eq!(backend.position, (1.0, 2.0));
        assert_eq!(backend.size, (480.0, 320.0));
        assert_eq!(backend.floor, FloorRef::Ground);

        let note = workspace
            .nodes
            .iter()
            .find(|n| n.id == Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap())
            .unwrap();
        match &note.kind {
            NodeKind::Note(payload) => {
                assert_eq!(payload.markdown, "# TODO\n- [ ] ship it");
                assert_eq!(payload.color, "yellow");
            }
            other => panic!("expected a Note node, got {other:?}"),
        }

        let edge = &workspace.edges[0];
        assert_eq!(
            edge.source,
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
        );
        assert_eq!(
            edge.target,
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap()
        );
        assert!(edge.is_visual_only());

        assert_eq!(workspace.canvas.zoom, 1.25);
    }
}
