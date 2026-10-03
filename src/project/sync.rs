//! Revision tracking and safe synchronization between an in-memory copy of a
//! file (a file-backed Note's Markdown, an editor buffer) and the file on
//! disk. A [`FileRevision`] is a content hash, not a timestamp: two writes
//! within one mtime tick, or a `touch` that changes nothing, can't confuse
//! it. Timestamps are only ever used as a cheap "maybe changed, go hash it"
//! hint by the polling watcher (`App::sync_project_files`).
//!
//! [`reconcile`] is the whole policy, as one pure function over three
//! revisions — the last synced *base*, the current *local* content, and the
//! current *disk* content — so every case (local edit, external edit, both,
//! deletion) is decided in one place and unit tested without GTK, a timer,
//! or a real filesystem. The one rule it encodes: a concurrent external
//! change is never silently overwritten — when both sides moved, it reports
//! a conflict and lets the user choose.
//!
//! Everything here is written against [`ProjectFilesystem`], not the local
//! disk, so file-backed Notes work unchanged once a project lives behind
//! SSH/Docker.

use super::fs::{FsError, FsResult, ProjectFilesystem};
use super::path::ProjectPath;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// A content hash identifying one exact version of a file's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileRevision(pub String);

impl FileRevision {
    pub fn of(bytes: &[u8]) -> FileRevision {
        let digest = Sha256::digest(bytes);
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        FileRevision(hex)
    }

    /// The first 12 hex digits — enough to tell revisions apart in UI and
    /// CLI output.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(12)]
    }
}

impl fmt::Display for FileRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The current on-disk state of a synced file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskState {
    Missing,
    Present {
        revision: FileRevision,
        content: Vec<u8>,
    },
}

impl DiskState {
    pub fn revision(&self) -> Option<&FileRevision> {
        match self {
            DiskState::Missing => None,
            DiskState::Present { revision, .. } => Some(revision),
        }
    }
}

pub fn disk_state(fs: &dyn ProjectFilesystem, path: &ProjectPath) -> FsResult<DiskState> {
    match fs.read(path) {
        Ok(content) => Ok(DiskState::Present {
            revision: FileRevision::of(&content),
            content,
        }),
        Err(FsError::NotFound(_)) => Ok(DiskState::Missing),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Local content and the file both changed since the last sync, to
    /// different content.
    BothChanged,
    /// The file was deleted (or moved away) after it had been synced.
    DeletedExternally,
}

/// What a sync pass should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    /// Nothing changed on either side.
    InSync,
    /// Only the local copy changed: write it to disk.
    WriteLocal,
    /// Only the file changed: replace the local copy with it.
    LoadDisk,
    /// Both changed, to identical content: just record the new base.
    Converged,
    /// Both sides changed differently (or the file vanished): do nothing
    /// automatically, surface it to the user.
    Conflict(ConflictKind),
}

/// Decides how to bring `local` and `disk` back in sync, given `base` — the
/// revision both last agreed on (`None` when the local copy has never been
/// synced to this file). `disk` is `None` when the file doesn't exist.
pub fn reconcile(
    base: Option<&FileRevision>,
    local: &FileRevision,
    disk: Option<&FileRevision>,
) -> SyncAction {
    let Some(disk) = disk else {
        return if base.is_none() {
            // Never synced and nothing on disk: creating the file is safe.
            SyncAction::WriteLocal
        } else {
            SyncAction::Conflict(ConflictKind::DeletedExternally)
        };
    };
    if local == disk {
        return if base == Some(disk) {
            SyncAction::InSync
        } else {
            SyncAction::Converged
        };
    }
    let local_changed = base != Some(local);
    let disk_changed = base != Some(disk);
    match (local_changed, disk_changed) {
        // Unreachable in practice (local == disk was handled above when
        // neither changed), kept total rather than panicking.
        (false, false) => SyncAction::InSync,
        (true, false) => SyncAction::WriteLocal,
        (false, true) => SyncAction::LoadDisk,
        (true, true) => SyncAction::Conflict(ConflictKind::BothChanged),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The file on disk is no longer the revision the caller based its
    /// edit on — someone else changed it. Nothing was written.
    Conflict {
        current: Option<FileRevision>,
    },
    Fs(FsError),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Conflict {
                current: Some(current),
            } => write!(
                f,
                "the file changed on disk (now revision {}); reload it or overwrite explicitly",
                current.short()
            ),
            WriteError::Conflict { current: None } => {
                write!(
                    f,
                    "the file was deleted on disk; reload or recreate it explicitly"
                )
            }
            WriteError::Fs(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for WriteError {}

/// Writes `contents` only if the file is still at `expected` (`None` meaning
/// "must not exist yet"), and returns the new revision. The check and the
/// write are as close together as one host round-trip allows; this is the
/// optimistic-concurrency guard every save path uses so a concurrent
/// external change is reported, never clobbered.
pub fn write_checked(
    fs: &dyn ProjectFilesystem,
    path: &ProjectPath,
    expected: Option<&FileRevision>,
    contents: &[u8],
) -> Result<FileRevision, WriteError> {
    let current = disk_state(fs, path).map_err(WriteError::Fs)?;
    if current.revision() != expected {
        return Err(WriteError::Conflict {
            current: current.revision().cloned(),
        });
    }
    fs.write(path, contents).map_err(WriteError::Fs)?;
    Ok(FileRevision::of(contents))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::fs::LocalProject;
    use tempfile::tempdir;

    fn rev(text: &str) -> FileRevision {
        FileRevision::of(text.as_bytes())
    }

    #[test]
    fn revisions_are_content_hashes() {
        assert_eq!(rev("a"), rev("a"));
        assert_ne!(rev("a"), rev("b"));
        assert_eq!(rev("").0.len(), 64);
        assert_eq!(rev("a").short().len(), 12);
    }

    #[test]
    fn reconcile_covers_every_case() {
        let (a, b, c) = (rev("a"), rev("b"), rev("c"));
        assert_eq!(reconcile(Some(&a), &a, Some(&a)), SyncAction::InSync);
        assert_eq!(reconcile(Some(&a), &b, Some(&a)), SyncAction::WriteLocal);
        assert_eq!(reconcile(Some(&a), &a, Some(&b)), SyncAction::LoadDisk);
        assert_eq!(reconcile(Some(&a), &b, Some(&b)), SyncAction::Converged);
        assert_eq!(
            reconcile(Some(&a), &b, Some(&c)),
            SyncAction::Conflict(ConflictKind::BothChanged)
        );
        assert_eq!(
            reconcile(Some(&a), &a, None),
            SyncAction::Conflict(ConflictKind::DeletedExternally)
        );
        assert_eq!(reconcile(None, &a, None), SyncAction::WriteLocal);
        // Never synced but a different file already exists: never clobber.
        assert_eq!(
            reconcile(None, &a, Some(&b)),
            SyncAction::Conflict(ConflictKind::BothChanged)
        );
        assert_eq!(reconcile(None, &a, Some(&a)), SyncAction::Converged);
    }

    #[test]
    fn write_checked_refuses_to_clobber_a_concurrent_change() {
        let tmp = tempdir().unwrap();
        let fs = LocalProject::new(tmp.path());
        let path = ProjectPath::parse("notes/plan.md").unwrap();

        // Creating requires the file not to exist yet.
        let first = write_checked(&fs, &path, None, b"v1").unwrap();
        assert_eq!(first, rev("v1"));
        assert!(matches!(
            write_checked(&fs, &path, None, b"again"),
            Err(WriteError::Conflict { .. })
        ));

        // An external edit lands between our read and our save.
        std::fs::write(tmp.path().join("notes/plan.md"), b"external").unwrap();
        let error = write_checked(&fs, &path, Some(&first), b"mine").unwrap_err();
        assert_eq!(
            error,
            WriteError::Conflict {
                current: Some(rev("external"))
            }
        );
        assert_eq!(
            std::fs::read(tmp.path().join("notes/plan.md")).unwrap(),
            b"external"
        );

        // Basing the save on the revision actually on disk succeeds.
        let second = write_checked(&fs, &path, Some(&rev("external")), b"mine").unwrap();
        assert_eq!(second, rev("mine"));
    }

    #[test]
    fn disk_state_reports_missing_files() {
        let tmp = tempdir().unwrap();
        let fs = LocalProject::new(tmp.path());
        let path = ProjectPath::parse("gone.md").unwrap();
        assert_eq!(disk_state(&fs, &path).unwrap(), DiskState::Missing);
        std::fs::write(tmp.path().join("gone.md"), b"x").unwrap();
        assert_eq!(disk_state(&fs, &path).unwrap().revision(), Some(&rev("x")));
    }
}
