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
//! - 3: the current multi-workspace shape (`workspaces`/`active_workspace`/
//!   `custom_roles`). Files written before this module existed have this
//!   shape but no `"schema_version"` key at all; they are treated as 3,
//!   since 3 is the only version this shape has ever had.
//!
//! A `schema_version` *greater* than [`CURRENT_SCHEMA_VERSION`] means the
//! file was written by a newer duet. Rather than guess at a shape it has
//! never seen, this module refuses to touch it: the original file is backed
//! up untouched and nothing is migrated, so upgrading duet (not editing the
//! file by hand) is the only way to recover it.

use crate::agent::Agent;
use crate::store::{
    CanvasRecord, LinkRecord, SessionRecord, StickyNoteRecord, Store, WorkspaceRecord,
};
use serde::Deserialize;
use std::path::Path;
use uuid::Uuid;

/// The schema version this binary reads and writes. Bump this and add a
/// migration step below whenever `Store`'s on-disk shape changes.
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

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
    cwd: std::path::PathBuf,
    agent: Agent,
    claude_session_id: Option<Uuid>,
    claude_account: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyStore {
    tabs: Vec<LegacyTabRecord>,
}

/// The pre-workspace on-disk shape: a single flat canvas, no `workspaces`
/// key. Kept only to migrate files written before workspaces existed.
#[derive(Debug, Deserialize)]
struct PreWorkspaceStore {
    #[serde(default)]
    sessions: Vec<SessionRecord>,
    #[serde(default)]
    notes: Vec<StickyNoteRecord>,
    #[serde(default)]
    links: Vec<LinkRecord>,
    #[serde(default)]
    canvas: CanvasRecord,
}

const GRID_COLUMNS: f64 = 3.0;
const GRID_CELL_WIDTH: f64 = 520.0;
const GRID_CELL_HEIGHT: f64 = 360.0;
const DEFAULT_NODE_SIZE: (f64, f64) = (480.0, 320.0);

/// Name and root directory given to a workspace created by migrating an
/// older single-canvas store, so existing users don't lose their canvas.
fn default_workspace_root() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"))
}

/// Wraps one flat canvas's worth of records into a `Store` containing a
/// single "Default" workspace, used by both migration paths below. Always
/// stamped at the current schema version — migrating *is* arriving at the
/// current shape.
fn wrap_single_workspace(
    sessions: Vec<SessionRecord>,
    notes: Vec<StickyNoteRecord>,
    links: Vec<LinkRecord>,
    canvas: CanvasRecord,
) -> Store {
    let id = Uuid::new_v4();
    Store::new(
        vec![WorkspaceRecord {
            id,
            name: "Default".to_string(),
            root_dir: default_workspace_root(),
            sessions,
            notes,
            links,
            canvas,
        }],
        Some(id),
        Vec::new(),
    )
}

fn migrate_legacy(legacy: LegacyStore) -> Store {
    let sessions = legacy
        .tabs
        .into_iter()
        .enumerate()
        .map(|(index, tab)| {
            let column = (index as f64) % GRID_COLUMNS;
            let row = (index as f64 / GRID_COLUMNS).floor();
            SessionRecord {
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

    // Schema 3: the current shape. A `schema_version` key is only present on
    // files this module itself wrote; its absence just means "written before
    // this field existed", which this shape has always meant schema 3.
    if obj.contains_key("workspaces") {
        if let Some(version) = declared_schema_version(obj)
            && version > CURRENT_SCHEMA_VERSION
        {
            return backup_future_schema_file(path, version);
        }
        return match serde_json::from_value::<Store>(value) {
            Ok(store) => (store, None),
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
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn current_format_without_explicit_schema_version_defaults_to_current() {
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
    }

    #[test]
    fn current_format_with_explicit_matching_schema_version_loads_with_no_warning() {
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
        assert_eq!(store.schema_version, 3);
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
                "sessions": [],
                "notes": [],
                "links": [],
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
    fn loading_old_tabs_shape_migrates_into_one_default_workspace_at_current_schema() {
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
        assert_eq!(workspace.sessions.len(), 2);
        assert_ne!(workspace.sessions[0].id, workspace.sessions[1].id);
        assert_eq!(workspace.sessions[0].position, (0.0, 0.0));
        assert_eq!(workspace.sessions[1].position, (520.0, 0.0));
        assert_eq!(workspace.canvas.zoom, 1.0);
    }

    /// The exact scenario schema 2 -> 3 migration exists for: a user
    /// upgrading from the single-canvas (pre-workspace) version must not
    /// lose their canvas — it becomes one workspace, not an empty store.
    #[test]
    fn loading_pre_workspace_single_canvas_shape_migrates_into_one_default_workspace() {
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
        assert_eq!(workspace.sessions.len(), 1);
        assert_eq!(workspace.sessions[0].name, "web");
        assert_eq!(workspace.canvas.zoom, 2.0);
    }

    /// Milestone 3 added three `Agent` variants alongside the original
    /// `Claude`/`Codex` unit variants. A workspace saved by an older duet
    /// binary has `"agent": "Claude"`/`"agent": "Codex"` as bare JSON
    /// strings (serde's default unit-variant representation) — confirms
    /// that literal shape still deserializes under the expanded enum with
    /// zero migration code, i.e. existing saved sessions are preserved.
    #[test]
    fn old_bare_string_agent_values_still_deserialize() {
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
        assert_eq!(store.workspaces[0].sessions[0].agent, Agent::Claude);
        assert_eq!(store.workspaces[0].sessions[1].agent, Agent::Codex);
    }

    /// A store saved before milestone 4 has no `"custom_roles"` key and no
    /// session record has a `"role_id"` key at all — confirms both default
    /// to empty/`None` rather than failing to load.
    #[test]
    fn store_without_roles_still_loads_with_sessions_unassigned() {
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
        assert_eq!(store.workspaces[0].sessions[0].role_id, None);
        assert!(store.custom_roles.is_empty());
    }
}
