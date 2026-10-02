use crate::agent::Agent;
use crate::role::Role;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: Uuid,
    pub name: String,
    pub cwd: PathBuf,
    pub agent: Agent,
    pub claude_session_id: Option<Uuid>,
    pub claude_account: Option<String>,
    /// The role (if any) this session was assigned — an id into
    /// `role::builtin_roles()` or `Store::custom_roles`. `#[serde(default)]`
    /// so a store saved before milestone 4 still loads with every existing
    /// session simply unassigned.
    #[serde(default)]
    pub role_id: Option<Uuid>,
    pub position: (f64, f64),
    pub size: (f64, f64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StickyNoteRecord {
    pub id: Uuid,
    pub text: String,
    pub position: (f64, f64),
    pub size: (f64, f64),
    pub color: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinkRecord {
    pub source: Uuid,
    pub target: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CanvasRecord {
    pub zoom: f64,
    pub pan: (f64, f64),
}

impl Default for CanvasRecord {
    fn default() -> Self {
        CanvasRecord {
            zoom: 1.0,
            pan: (0.0, 0.0),
        }
    }
}

/// An independently persisted canvas/project context: its own sessions,
/// notes, links, and pan/zoom, plus a `root_dir` used only as the default
/// working directory suggested when creating a new session inside it (a
/// session's own `cwd` can still be anything — this is a convenience, not a
/// constraint).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: Uuid,
    pub name: String,
    pub root_dir: PathBuf,
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
    #[serde(default)]
    pub notes: Vec<StickyNoteRecord>,
    #[serde(default)]
    pub links: Vec<LinkRecord>,
    #[serde(default)]
    pub canvas: CanvasRecord,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub workspaces: Vec<WorkspaceRecord>,
    #[serde(default)]
    pub active_workspace: Option<Uuid>,
    /// User-created roles, global across every workspace (unlike sessions/
    /// notes/links). Built-in roles are never persisted — see
    /// `role::builtin_roles`.
    #[serde(default)]
    pub custom_roles: Vec<Role>,
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
fn default_workspace_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Wraps one flat canvas's worth of records into a `Store` containing a
/// single "Default" workspace, used by both migration paths below.
fn wrap_single_workspace(
    sessions: Vec<SessionRecord>,
    notes: Vec<StickyNoteRecord>,
    links: Vec<LinkRecord>,
    canvas: CanvasRecord,
) -> Store {
    let id = Uuid::new_v4();
    Store {
        workspaces: vec![WorkspaceRecord {
            id,
            name: "Default".to_string(),
            root_dir: default_workspace_root(),
            sessions,
            notes,
            links,
            canvas,
        }],
        active_workspace: Some(id),
        custom_roles: Vec::new(),
    }
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

fn backup_corrupt_file(path: &Path, reason: &str) -> (Store, Option<String>) {
    let backup = path.with_extension("corrupt.json");
    let backup_note = match std::fs::copy(path, &backup) {
        Ok(_) => format!(" A backup was saved to {}.", backup.display()),
        Err(_) => String::new(),
    };
    (
        Store::default(),
        Some(format!(
            "Couldn't read the saved workspace ({reason}).{backup_note}"
        )),
    )
}

impl Store {
    #[cfg(test)]
    pub fn load(path: &Path) -> Store {
        Self::load_with_warning(path).0
    }

    pub fn load_with_warning(path: &Path) -> (Store, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                // Try to parse as JSON value to detect format
                let value: Result<serde_json::Value, _> = serde_json::from_str(&contents);

                if let Ok(value) = value {
                    // If it has "tabs" key, it's legacy format
                    if value.get("tabs").is_some() {
                        match serde_json::from_value::<LegacyStore>(value) {
                            Ok(legacy) => return (migrate_legacy(legacy), None),
                            Err(error) => {
                                return backup_corrupt_file(path, &error.to_string());
                            }
                        }
                    }

                    // Check if it's a JSON object with at least one recognized key
                    if let Some(obj) = value.as_object() {
                        if obj.contains_key("workspaces") {
                            // Current format; try to deserialize directly.
                            if let Ok(store) = serde_json::from_value::<Store>(value) {
                                return (store, None);
                            }
                        } else if obj.contains_key("sessions")
                            || obj.contains_key("notes")
                            || obj.contains_key("links")
                            || obj.contains_key("canvas")
                        {
                            // Pre-workspace single-canvas format.
                            if let Ok(pre) = serde_json::from_value::<PreWorkspaceStore>(value) {
                                return (
                                    wrap_single_workspace(
                                        pre.sessions,
                                        pre.notes,
                                        pre.links,
                                        pre.canvas,
                                    ),
                                    None,
                                );
                            }
                        } else {
                            // Valid JSON object but has no recognized keys and no "tabs"
                            // This is ambiguous/truncated data — treat as corrupt
                            return backup_corrupt_file(path, "unrecognized JSON structure");
                        }
                    } else {
                        // JSON is not an object (e.g., array, string, number, null)
                        return backup_corrupt_file(path, "JSON is not an object");
                    }
                }

                // If JSON parsing itself failed, it's corrupt
                backup_corrupt_file(path, "Invalid JSON")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Store::default(), None),
            Err(error) => (
                Store::default(),
                Some(format!("Couldn't read the saved workspace: {error}")),
            ),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).expect("Store always serializes");
        let temporary = path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(temporary, path)
    }
}

pub fn default_store_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("no data directory available on this platform"))?
        .join("duet");
    Ok(dir.join("store.json"))
}

pub fn default_accounts_dir() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("no data directory available on this platform"))?
        .join("duet")
        .join("accounts");
    Ok(dir)
}

/// Where `control.rs`'s Unix-domain control socket listens — one per user,
/// the same single-instance assumption `default_store_path` already makes.
/// Prefers `XDG_RUNTIME_DIR` (short, tmpfs-backed, exactly what a runtime
/// socket is for) over the data dir, both because it's the textbook-correct
/// spot and because a `sockaddr_un` path is capped at ~108 bytes on Linux —
/// a long `$HOME`-derived data dir can overflow that, where a runtime dir
/// (typically `/run/user/<uid>`) essentially never does.
pub fn default_control_socket_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::runtime_dir()
        .or_else(dirs::data_dir)
        .ok_or_else(|| anyhow::anyhow!("no runtime or data directory available on this platform"))?
        .join("duet");
    Ok(dir.join("control.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_record() -> SessionRecord {
        SessionRecord {
            id: Uuid::nil(),
            name: "web".to_string(),
            cwd: PathBuf::from("/home/fernando/web"),
            agent: Agent::Claude,
            claude_session_id: Some(Uuid::nil()),
            claude_account: Some("work".to_string()),
            role_id: None,
            position: (10.0, 20.0),
            size: (480.0, 320.0),
        }
    }

    fn sample_workspace(sessions: Vec<SessionRecord>) -> WorkspaceRecord {
        WorkspaceRecord {
            id: Uuid::nil(),
            name: "web".to_string(),
            root_dir: PathBuf::from("/home/fernando"),
            sessions,
            notes: vec![StickyNoteRecord {
                id: Uuid::nil(),
                text: "hello".to_string(),
                position: (1.0, 2.0),
                size: (200.0, 150.0),
                color: "yellow".to_string(),
            }],
            links: vec![LinkRecord {
                source: Uuid::nil(),
                target: Uuid::nil(),
            }],
            canvas: CanvasRecord {
                zoom: 1.5,
                pan: (3.0, 4.0),
            },
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        let workspace = sample_workspace(vec![sample_record()]);
        let role = Role {
            id: Uuid::nil(),
            name: "Custom".to_string(),
            instructions: "Be custom.".to_string(),
            icon: Some("face-smile-symbolic".to_string()),
            accent: Some("blue".to_string()),
        };
        let store = Store {
            workspaces: vec![workspace.clone()],
            active_workspace: Some(workspace.id),
            custom_roles: vec![role.clone()],
        };
        store.save(&path).unwrap();
        let loaded = Store::load(&path);
        assert_eq!(loaded.workspaces, vec![workspace.clone()]);
        assert_eq!(loaded.active_workspace, Some(workspace.id));
        assert_eq!(loaded.custom_roles, vec![role]);
    }

    /// A store saved before milestone 4 has no `"custom_roles"` key and no
    /// session record has a `"role_id"` key at all — confirms both default
    /// to empty/`None` rather than failing to load.
    #[test]
    fn store_without_roles_still_loads_with_sessions_unassigned() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        std::fs::write(
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
        )
        .unwrap();
        let store = Store::load(&path);
        assert_eq!(store.workspaces[0].sessions[0].role_id, None);
        assert!(store.custom_roles.is_empty());
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("store.json");
        let store = Store {
            workspaces: vec![sample_workspace(vec![sample_record()])],
            active_workspace: None,
            custom_roles: Vec::new(),
        };
        store.save(&path).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        assert!(Store::load(&path).workspaces.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(Store::load(&path).workspaces.is_empty());
        assert!(path.with_extension("corrupt.json").exists());
    }

    #[test]
    fn loading_old_tabs_shape_migrates_into_one_default_workspace() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        std::fs::write(
            &path,
            r#"{"tabs": [
                {"name": "a", "cwd": "/tmp", "agent": "Codex", "claude_session_id": null, "claude_account": null, "codex_used": true},
                {"name": "b", "cwd": "/tmp", "agent": "Codex", "claude_session_id": null, "claude_account": null, "codex_used": true}
            ]}"#,
        )
        .unwrap();
        let store = Store::load(&path);
        assert_eq!(store.workspaces.len(), 1);
        let workspace = &store.workspaces[0];
        assert_eq!(store.active_workspace, Some(workspace.id));
        assert_eq!(workspace.sessions.len(), 2);
        assert_ne!(workspace.sessions[0].id, workspace.sessions[1].id);
        assert_eq!(workspace.sessions[0].position, (0.0, 0.0));
        assert_eq!(workspace.sessions[1].position, (520.0, 0.0));
        assert_eq!(workspace.canvas.zoom, 1.0);
    }

    /// The exact scenario the migration exists for: a user upgrading from the
    /// single-canvas (pre-workspace) version must not lose their canvas — it
    /// becomes one workspace, not an empty store.
    #[test]
    fn loading_pre_workspace_single_canvas_shape_migrates_into_one_default_workspace() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        std::fs::write(
            &path,
            r#"{
                "sessions": [{"id": "00000000-0000-0000-0000-000000000000", "name": "web", "cwd": "/tmp", "agent": "Codex", "claude_session_id": null, "claude_account": null, "position": [10.0, 20.0], "size": [480.0, 320.0]}],
                "notes": [],
                "links": [],
                "canvas": {"zoom": 2.0, "pan": [5.0, 6.0]}
            }"#,
        )
        .unwrap();
        let store = Store::load(&path);
        assert_eq!(store.workspaces.len(), 1);
        let workspace = &store.workspaces[0];
        assert_eq!(store.active_workspace, Some(workspace.id));
        assert_eq!(workspace.sessions.len(), 1);
        assert_eq!(workspace.sessions[0].name, "web");
        assert_eq!(workspace.canvas.zoom, 2.0);
    }

    #[test]
    fn load_of_ambiguous_json_object_is_treated_as_corrupt() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        std::fs::write(&path, "{}").unwrap();
        assert!(Store::load(&path).workspaces.is_empty());
        assert!(path.with_extension("corrupt.json").exists());
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
        std::fs::write(
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
        )
        .unwrap();
        let store = Store::load(&path);
        assert_eq!(store.workspaces[0].sessions[0].agent, Agent::Claude);
        assert_eq!(store.workspaces[0].sessions[1].agent, Agent::Codex);
    }

    /// A custom-provider session's `Agent::Custom { program, args }` carries
    /// its own metadata inside the enum variant, serializing as a JSON
    /// object rather than a bare string — confirms it round-trips through a
    /// real save+load, not just `agent.rs`'s own unit tests.
    #[test]
    fn custom_agent_round_trips_through_save_and_load() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        let mut record = sample_record();
        record.agent = Agent::Custom {
            program: "mytool".to_string(),
            args: vec!["--flag".to_string()],
        };
        let workspace = sample_workspace(vec![record.clone()]);
        let store = Store {
            workspaces: vec![workspace.clone()],
            active_workspace: Some(workspace.id),
            custom_roles: Vec::new(),
        };
        store.save(&path).unwrap();
        let loaded = Store::load(&path);
        assert_eq!(loaded.workspaces[0].sessions[0].agent, record.agent);
    }
}
