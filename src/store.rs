use crate::agent::Agent;
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

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
    #[serde(default)]
    pub notes: Vec<StickyNoteRecord>,
    #[serde(default)]
    pub links: Vec<LinkRecord>,
    #[serde(default)]
    pub canvas: CanvasRecord,
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

const GRID_COLUMNS: f64 = 3.0;
const GRID_CELL_WIDTH: f64 = 520.0;
const GRID_CELL_HEIGHT: f64 = 360.0;
const DEFAULT_NODE_SIZE: (f64, f64) = (480.0, 320.0);

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
                position: (column * GRID_CELL_WIDTH, row * GRID_CELL_HEIGHT),
                size: DEFAULT_NODE_SIZE,
            }
        })
        .collect();
    Store {
        sessions,
        notes: Vec::new(),
        links: Vec::new(),
        canvas: CanvasRecord::default(),
    }
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
                                let backup = path.with_extension("corrupt.json");
                                let backup_note = match std::fs::copy(path, &backup) {
                                    Ok(_) => format!(" A backup was saved to {}.", backup.display()),
                                    Err(_) => String::new(),
                                };
                                return (
                                    Store::default(),
                                    Some(format!(
                                        "Couldn't read the saved workspace ({error}).{backup_note}"
                                    )),
                                );
                            }
                        }
                    }
                    // Otherwise try new format
                    if let Ok(store) = serde_json::from_value::<Store>(value) {
                        return (store, None);
                    }
                }

                // If all else fails, it's corrupt
                let error = "Invalid JSON or unrecognized format";
                let backup = path.with_extension("corrupt.json");
                let backup_note = match std::fs::copy(path, &backup) {
                    Ok(_) => format!(" A backup was saved to {}.", backup.display()),
                    Err(_) => String::new(),
                };
                (
                    Store::default(),
                    Some(format!(
                        "Couldn't read the saved workspace ({error}).{backup_note}"
                    )),
                )
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
            position: (10.0, 20.0),
            size: (480.0, 320.0),
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        let store = Store {
            sessions: vec![sample_record()],
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
        };
        store.save(&path).unwrap();
        let loaded = Store::load(&path);
        assert_eq!(loaded.sessions, vec![sample_record()]);
        assert_eq!(loaded.canvas.zoom, 1.5);
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("store.json");
        let store = Store {
            sessions: vec![sample_record()],
            notes: vec![],
            links: vec![],
            canvas: CanvasRecord::default(),
        };
        store.save(&path).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        assert!(Store::load(&path).sessions.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(Store::load(&path).sessions.is_empty());
        assert!(path.with_extension("corrupt.json").exists());
    }

    #[test]
    fn loading_old_tabs_shape_migrates_with_grid_positions_and_fresh_ids() {
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
        assert_eq!(store.sessions.len(), 2);
        assert_ne!(store.sessions[0].id, store.sessions[1].id);
        assert_eq!(store.sessions[0].position, (0.0, 0.0));
        assert_eq!(store.sessions[1].position, (520.0, 0.0));
        assert_eq!(store.canvas.zoom, 1.0);
    }
}
