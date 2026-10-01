use std::io;
use std::path::PathBuf;

pub const DEFAULT_ACCOUNT: &str = "default";

pub struct AccountStore {
    root: PathBuf,
}

impl AccountStore {
    pub fn new(root: PathBuf) -> Self {
        AccountStore { root }
    }

    pub fn config_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn ensure(&self, name: &str) -> io::Result<PathBuf> {
        validate_name(name)?;
        let dir = self.config_dir(name);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn list(&self) -> io::Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = std::fs::read_dir(&self.root)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(names)
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        validate_name(name)?;
        let dir = self.config_dir(name);
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "account names may only contain letters, numbers, '-' and '_'",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn ensure_creates_directory_and_list_finds_it_sorted() {
        let tmp = tempdir().unwrap();
        let store = AccountStore::new(tmp.path().join("accounts"));
        store.ensure("work").unwrap();
        store.ensure("personal").unwrap();
        assert_eq!(
            store.list().unwrap(),
            vec!["personal".to_string(), "work".to_string()]
        );
    }

    #[test]
    fn list_on_missing_root_is_empty() {
        let tmp = tempdir().unwrap();
        let store = AccountStore::new(tmp.path().join("nonexistent"));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn config_dir_joins_root_and_name() {
        let store = AccountStore::new(PathBuf::from("/tmp/duet-test-root"));
        assert_eq!(
            store.config_dir("work"),
            PathBuf::from("/tmp/duet-test-root/work")
        );
    }

    #[test]
    fn ensure_is_idempotent() {
        let tmp = tempdir().unwrap();
        let store = AccountStore::new(tmp.path().join("accounts"));
        store.ensure("work").unwrap();
        store.ensure("work").unwrap();
        assert_eq!(store.list().unwrap(), vec!["work".to_string()]);
    }

    #[test]
    fn remove_deletes_only_the_named_account() {
        let tmp = tempdir().unwrap();
        let store = AccountStore::new(tmp.path().join("accounts"));
        store.ensure("personal").unwrap();
        store.ensure("work").unwrap();
        store.remove("personal").unwrap();
        assert_eq!(store.list().unwrap(), vec!["work".to_string()]);
    }

    #[test]
    fn unsafe_account_names_are_rejected() {
        let tmp = tempdir().unwrap();
        let store = AccountStore::new(tmp.path().join("accounts"));
        assert!(store.ensure("../escape").is_err());
        assert!(store.ensure("spaces are ambiguous").is_err());
        assert!(store.ensure("work-2_personal").is_ok());
    }
}
