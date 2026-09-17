use crate::agent::Agent;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabRecord {
    pub name: String,
    pub cwd: PathBuf,
    pub agent: Agent,
    pub claude_session_id: Option<Uuid>,
    pub claude_account: Option<String>,
    pub codex_used: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub tabs: Vec<TabRecord>,
}

impl Store {
    pub fn load(path: &Path) -> Store {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Store::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).expect("Store always serializes");
        std::fs::write(path, json)
    }
}

pub fn default_store_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("no data directory available on this platform"))?
        .join("duet");
    Ok(dir.join("tabs.json"))
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

    fn sample_record() -> TabRecord {
        TabRecord {
            name: "web".to_string(),
            cwd: PathBuf::from("/home/fernando/web"),
            agent: Agent::Claude,
            claude_session_id: Some(Uuid::nil()),
            claude_account: Some("work".to_string()),
            codex_used: false,
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("tabs.json");
        let store = Store {
            tabs: vec![sample_record()],
        };
        store.save(&path).unwrap();
        let loaded = Store::load(&path);
        assert_eq!(loaded.tabs, vec![sample_record()]);
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("tabs.json");
        let store = Store {
            tabs: vec![sample_record()],
        };
        store.save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        assert!(Store::load(&path).tabs.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("tabs.json");
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(Store::load(&path).tabs.is_empty());
    }
}
