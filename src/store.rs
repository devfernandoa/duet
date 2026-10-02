use crate::environment::EnvironmentKind;
use crate::model::{EdgeRecord, NodeRecord};
use crate::role::Role;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

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

/// An independently persisted canvas/project context: its own nodes, edges,
/// and pan/zoom, plus a `root_dir` used only as the default working
/// directory suggested when creating a new terminal node inside it (a
/// node's own `cwd` can still be anything — this is a convenience, not a
/// constraint).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: Uuid,
    pub name: String,
    pub root_dir: PathBuf,
    #[serde(default)]
    pub nodes: Vec<NodeRecord>,
    #[serde(default)]
    pub edges: Vec<EdgeRecord>,
    #[serde(default)]
    pub canvas: CanvasRecord,
    /// The default runtime a new terminal in this workspace gets, unless
    /// something overrides it. `#[serde(default)]` so every workspace saved
    /// before Milestone 2 loads as `LocalPty`.
    #[serde(default)]
    pub environment: EnvironmentKind,
    /// Cosmetic workspace metadata, both `#[serde(default)]`'d to `None` for
    /// a workspace saved before this field existed. No UI sets these to
    /// `Some` yet — they exist so the persisted shape already has room for
    /// the color/icon picker a later pass can add without another schema
    /// migration.
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    /// Unix-epoch seconds. `0` for a workspace migrated from a schema that
    /// never recorded this — an honest "unknown", not a fabricated creation
    /// time.
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_opened: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Store {
    /// Explicit on-disk schema version — see `migration.rs`, the only module
    /// that reads or writes this field directly. `#[serde(default = ...)]`
    /// so a file written before this field existed (every shipped version so
    /// far) loads as the current version rather than failing: that shape has
    /// only ever meant one schema.
    #[serde(default = "crate::migration::current_schema_version_default")]
    pub schema_version: u32,
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

impl Default for Store {
    fn default() -> Self {
        Store::new(Vec::new(), None, Vec::new())
    }
}

impl Store {
    /// Builds a `Store` at [`crate::migration::CURRENT_SCHEMA_VERSION`] — the
    /// one place that stamps `schema_version` on a freshly-built `Store`, so
    /// no caller (or migration) can forget to tag what it writes.
    pub fn new(
        workspaces: Vec<WorkspaceRecord>,
        active_workspace: Option<Uuid>,
        custom_roles: Vec<Role>,
    ) -> Store {
        Store {
            schema_version: crate::migration::CURRENT_SCHEMA_VERSION,
            workspaces,
            active_workspace,
            custom_roles,
        }
    }

    #[cfg(test)]
    pub fn load(path: &Path) -> Store {
        Self::load_with_warning(path).0
    }

    /// Reads the store file and migrates it to the current shape, whatever
    /// vintage it turns out to be (see `migration::load`). A missing file is
    /// simply "nothing saved yet", not an error; any other read failure is
    /// reported but still yields a usable (empty) `Store` rather than a
    /// crash.
    pub fn load_with_warning(path: &Path) -> (Store, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(contents) => crate::migration::load(path, &contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Store::default(), None),
            Err(error) => (
                Store::default(),
                Some(format!("Couldn't read the saved workspace: {error}")),
            ),
        }
    }

    /// Splits a loaded store's workspaces into the one that should become
    /// live and everything else that stays dormant — pure data logic (no
    /// GTK/runtime involved), so `App::restore` only has to decide what to
    /// *do* with the split, not how to compute it. The active workspace is
    /// whichever `active_workspace` id names; if that id is missing or
    /// doesn't match any saved workspace (deleted since the last save, or an
    /// older file that never recorded one), it falls back to the first
    /// workspace in storage order. `None` when there are no saved workspaces
    /// at all — a fresh install, which the caller treats as "nothing to
    /// restore" rather than conjuring a placeholder.
    pub fn resolve_active_workspace(self) -> Option<(WorkspaceRecord, Vec<WorkspaceRecord>)> {
        if self.workspaces.is_empty() {
            return None;
        }
        let mut workspaces = self.workspaces;
        let active_index = self
            .active_workspace
            .and_then(|id| workspaces.iter().position(|w| w.id == id))
            .unwrap_or(0);
        let active = workspaces.remove(active_index);
        Some((active, workspaces))
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
    use crate::agent::Agent;
    use crate::model::{FloorRef, NodeKind, NotePayload, NoteViewMode, TerminalPayload};
    use tempfile::tempdir;

    fn sample_terminal_node() -> NodeRecord {
        NodeRecord {
            id: Uuid::nil(),
            floor: FloorRef::Ground,
            position: (10.0, 20.0),
            size: (480.0, 320.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Terminal(TerminalPayload {
                name: "web".to_string(),
                cwd: PathBuf::from("/home/fernando/web"),
                agent: Agent::Claude,
                claude_session_id: Some(Uuid::nil()),
                claude_account: Some("work".to_string()),
                role_id: None,
                environment: EnvironmentKind::LocalPty,
            }),
        }
    }

    fn sample_workspace(nodes: Vec<NodeRecord>) -> WorkspaceRecord {
        WorkspaceRecord {
            id: Uuid::nil(),
            name: "web".to_string(),
            root_dir: PathBuf::from("/home/fernando"),
            nodes,
            edges: vec![EdgeRecord::visual(Uuid::nil(), Uuid::nil(), Uuid::nil())],
            canvas: CanvasRecord {
                zoom: 1.5,
                pan: (3.0, 4.0),
            },
            environment: EnvironmentKind::LocalPty,
            color: None,
            icon: None,
            created_at: 1_700_000_000,
            last_opened: 1_700_000_100,
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        let mut note = sample_terminal_node();
        note.id = Uuid::new_v4();
        note.kind = NodeKind::Note(NotePayload {
            markdown: "# hello".to_string(),
            color: "yellow".to_string(),
            view_mode: NoteViewMode::Edit,
        });
        let workspace = sample_workspace(vec![sample_terminal_node(), note]);
        let role = Role {
            id: Uuid::nil(),
            name: "Custom".to_string(),
            instructions: "Be custom.".to_string(),
            icon: Some("face-smile-symbolic".to_string()),
            accent: Some("blue".to_string()),
        };
        let store = Store::new(
            vec![workspace.clone()],
            Some(workspace.id),
            vec![role.clone()],
        );
        store.save(&path).unwrap();
        let loaded = Store::load(&path);
        assert_eq!(
            loaded.schema_version,
            crate::migration::CURRENT_SCHEMA_VERSION
        );
        assert_eq!(loaded.workspaces, vec![workspace.clone()]);
        assert_eq!(loaded.active_workspace, Some(workspace.id));
        assert_eq!(loaded.custom_roles, vec![role]);
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("store.json");
        let store = Store::new(
            vec![sample_workspace(vec![sample_terminal_node()])],
            None,
            Vec::new(),
        );
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
    fn resolve_active_workspace_is_none_when_nothing_was_saved() {
        assert!(Store::default().resolve_active_workspace().is_none());
    }

    fn named_workspace(name: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            id: Uuid::new_v4(),
            name: name.to_string(),
            root_dir: PathBuf::from("/home/fernando"),
            nodes: Vec::new(),
            edges: Vec::new(),
            canvas: CanvasRecord::default(),
            environment: EnvironmentKind::LocalPty,
            color: None,
            icon: None,
            created_at: 0,
            last_opened: 0,
        }
    }

    #[test]
    fn resolve_active_workspace_picks_the_workspace_named_by_active_workspace() {
        let (a, b) = (named_workspace("a"), named_workspace("b"));
        let store = Store::new(vec![a.clone(), b.clone()], Some(b.id), Vec::new());
        let (active, inactive) = store.resolve_active_workspace().unwrap();
        assert_eq!(active.id, b.id);
        assert_eq!(inactive, vec![a]);
    }

    #[test]
    fn resolve_active_workspace_falls_back_to_the_first_workspace_when_the_id_is_stale() {
        let (a, b) = (named_workspace("a"), named_workspace("b"));
        let store = Store::new(vec![a.clone(), b.clone()], Some(Uuid::new_v4()), Vec::new());
        let (active, inactive) = store.resolve_active_workspace().unwrap();
        assert_eq!(active.id, a.id);
        assert_eq!(inactive, vec![b.clone()]);

        let store = Store::new(vec![a.clone(), b.clone()], None, Vec::new());
        let (active, inactive) = store.resolve_active_workspace().unwrap();
        assert_eq!(active.id, a.id);
        assert_eq!(inactive, vec![b]);
    }

    /// A custom-provider session's `Agent::Custom { program, args }` carries
    /// its own metadata inside the enum variant, serializing as a JSON
    /// object rather than a bare string — confirms it round-trips through a
    /// real save+load, not just `agent.rs`'s own unit tests.
    #[test]
    fn custom_agent_round_trips_through_save_and_load() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        let mut node = sample_terminal_node();
        node.kind = NodeKind::Terminal(TerminalPayload {
            name: "custom".to_string(),
            cwd: PathBuf::from("/tmp"),
            agent: Agent::Custom {
                program: "mytool".to_string(),
                args: vec!["--flag".to_string()],
            },
            claude_session_id: None,
            claude_account: None,
            role_id: None,
            environment: EnvironmentKind::LocalPty,
        });
        let workspace = sample_workspace(vec![node.clone()]);
        let store = Store::new(vec![workspace.clone()], Some(workspace.id), Vec::new());
        store.save(&path).unwrap();
        let loaded = Store::load(&path);
        assert_eq!(loaded.workspaces[0].nodes[0], node);
    }
}
