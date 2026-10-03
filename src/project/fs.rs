//! The canonical project filesystem boundary. Every file operation Duet
//! performs on a project — the FileTree node, the embedded editor,
//! file-backed notes, `duetctl file`, Git, search — goes through
//! [`ProjectFilesystem`] (file I/O) and [`ProjectCommands`] (running a tool
//! such as `git` or `rg` inside the project root), never through `std::fs`
//! or `std::process::Command` directly. [`LocalProject`] is the only
//! implementation today; Milestone 13's SSH/Docker environments implement the
//! same two traits (OpenSSH `sftp`/`ssh host cmd`, `docker cp`/`docker exec`)
//! and every caller above keeps working unchanged.
//!
//! The traits are deliberately small: exactly the operations this milestone's
//! features need (metadata, list, read, write, run a tool), not a general VFS.
//! Higher-level behavior — search, Git, sync — is written once, generically,
//! on top of them (`search.rs`, `git.rs`, `sync.rs`).

use super::path::{PathError, ProjectPath};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// What `metadata` reports about one path. `modified` is milliseconds since
/// the Unix epoch when the host can tell (it's the cheap "did anything
/// change" signal `sync.rs`'s polling watcher checks before hashing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub path: ProjectPath,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<u64>,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: ProjectPath,
    pub kind: EntryKind,
}

impl DirEntry {
    pub fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    InvalidPath(PathError),
    NotFound(ProjectPath),
    /// The path is lexically inside the root but resolves (through a
    /// symlink) to somewhere outside it.
    OutsideRoot(ProjectPath),
    NotADirectory(ProjectPath),
    IsADirectory(ProjectPath),
    Io(String),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::InvalidPath(error) => write!(f, "{error}"),
            FsError::NotFound(path) => write!(f, "no such file or directory: {path}"),
            FsError::OutsideRoot(path) => {
                write!(f, "{path} resolves outside the project root (symlink)")
            }
            FsError::NotADirectory(path) => write!(f, "{path} is not a directory"),
            FsError::IsADirectory(path) => write!(f, "{path} is a directory"),
            FsError::Io(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for FsError {}

impl From<PathError> for FsError {
    fn from(error: PathError) -> Self {
        FsError::InvalidPath(error)
    }
}

pub type FsResult<T> = Result<T, FsError>;

/// File I/O inside one project root. Every path is a [`ProjectPath`], so an
/// implementation never has to defend against `..` itself — only against
/// host-specific escapes such as symlinks.
pub trait ProjectFilesystem {
    /// A human-readable description of where this project lives
    /// (`/home/u/project`, later `ssh:host:/srv/app`, ...). Display only.
    fn root_label(&self) -> String;
    fn metadata(&self, path: &ProjectPath) -> FsResult<FileMetadata>;
    /// The entries directly inside `path`, directories first, then by name.
    fn list_dir(&self, path: &ProjectPath) -> FsResult<Vec<DirEntry>>;
    fn read(&self, path: &ProjectPath) -> FsResult<Vec<u8>>;
    /// Replaces `path`'s content atomically (a reader never observes a
    /// half-written file), creating it — and any missing parent
    /// directories — if needed.
    fn write(&self, path: &ProjectPath, contents: &[u8]) -> FsResult<()>;

    fn exists(&self, path: &ProjectPath) -> bool {
        self.metadata(path).is_ok()
    }
}

/// One finished tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// `None` when the process was killed by a signal.
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

/// Runs a tool (`git`, `rg`) with the project root as its working
/// directory, wherever that root lives. Arguments are passed as a list,
/// never through a shell, so no caller can be tricked into shell injection
/// by a file name.
pub trait ProjectCommands {
    /// `Err` only when the program couldn't be started at all (for example
    /// it isn't installed — `ErrorKind::NotFound`); a program that ran and
    /// failed is an `Ok` with a non-zero `status`.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> std::io::Result<CommandOutput>;
}

/// Everything a project offers: its files and tools. Blanket-implemented —
/// a backend only ever implements the two halves.
pub trait Project: ProjectFilesystem + ProjectCommands {}
impl<T: ProjectFilesystem + ProjectCommands> Project for T {}

/// A project on the local disk, rooted at `root`.
#[derive(Debug, Clone)]
pub struct LocalProject {
    root: PathBuf,
}

impl LocalProject {
    pub fn new(root: impl Into<PathBuf>) -> LocalProject {
        LocalProject { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn canonical_root(&self) -> FsResult<PathBuf> {
        self.root.canonicalize().map_err(|error| {
            FsError::Io(format!(
                "project root {} is not accessible: {error}",
                self.root.display()
            ))
        })
    }

    /// The host path for `path`, after checking that it — or, for a path
    /// that doesn't exist yet, its nearest existing ancestor — really lies
    /// inside the canonical project root once symlinks are resolved.
    pub fn real_path(&self, path: &ProjectPath) -> FsResult<PathBuf> {
        let root = self.canonical_root()?;
        let joined = if path.is_root() {
            root.clone()
        } else {
            root.join(path.as_str())
        };
        let mut probe = joined.clone();
        let mut suffix: Vec<std::ffi::OsString> = Vec::new();
        loop {
            match probe.canonicalize() {
                Ok(real) => {
                    if !real.starts_with(&root) {
                        return Err(FsError::OutsideRoot(path.clone()));
                    }
                    let mut full = real;
                    for component in suffix.iter().rev() {
                        full.push(component);
                    }
                    return Ok(full);
                }
                Err(_) => {
                    let Some(name) = probe.file_name().map(|n| n.to_os_string()) else {
                        return Err(FsError::OutsideRoot(path.clone()));
                    };
                    suffix.push(name);
                    if !probe.pop() {
                        return Err(FsError::OutsideRoot(path.clone()));
                    }
                }
            }
        }
    }
}

fn io_error(path: &ProjectPath, error: std::io::Error) -> FsError {
    match error.kind() {
        std::io::ErrorKind::NotFound => FsError::NotFound(path.clone()),
        _ => FsError::Io(format!("{path}: {error}")),
    }
}

fn entry_kind(file_type: std::fs::FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

impl ProjectFilesystem for LocalProject {
    fn root_label(&self) -> String {
        self.root.display().to_string()
    }

    fn metadata(&self, path: &ProjectPath) -> FsResult<FileMetadata> {
        let real = self.real_path(path)?;
        let metadata = std::fs::metadata(&real).map_err(|error| io_error(path, error))?;
        Ok(FileMetadata {
            path: path.clone(),
            kind: entry_kind(metadata.file_type()),
            size: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64),
            readonly: metadata.permissions().readonly(),
        })
    }

    fn list_dir(&self, path: &ProjectPath) -> FsResult<Vec<DirEntry>> {
        let real = self.real_path(path)?;
        if !real.is_dir() {
            return Err(if real.exists() {
                FsError::NotADirectory(path.clone())
            } else {
                FsError::NotFound(path.clone())
            });
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&real).map_err(|error| io_error(path, error))? {
            let entry = entry.map_err(|error| io_error(path, error))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(child) = path.join(&name) else {
                continue;
            };
            // A symlink to a directory is listed as what it points to (so it
            // can be expanded) as long as that target stays in the root;
            // anything else that's a symlink stays `Symlink`.
            let raw_kind = entry
                .file_type()
                .map(entry_kind)
                .unwrap_or(EntryKind::Other);
            let kind = if raw_kind == EntryKind::Symlink {
                match self.real_path(&child) {
                    Ok(target) if target.is_dir() => EntryKind::Directory,
                    Ok(target) if target.is_file() => EntryKind::File,
                    _ => EntryKind::Symlink,
                }
            } else {
                raw_kind
            };
            entries.push(DirEntry {
                name,
                path: child,
                kind,
            });
        }
        entries.sort_by(|a, b| {
            let a_dir = a.kind == EntryKind::Directory;
            let b_dir = b.kind == EntryKind::Directory;
            b_dir
                .cmp(&a_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(entries)
    }

    fn read(&self, path: &ProjectPath) -> FsResult<Vec<u8>> {
        let real = self.real_path(path)?;
        if real.is_dir() {
            return Err(FsError::IsADirectory(path.clone()));
        }
        std::fs::read(&real).map_err(|error| io_error(path, error))
    }

    fn write(&self, path: &ProjectPath, contents: &[u8]) -> FsResult<()> {
        if path.is_root() {
            return Err(FsError::IsADirectory(path.clone()));
        }
        let real = self.real_path(path)?;
        if real.is_dir() {
            return Err(FsError::IsADirectory(path.clone()));
        }
        let parent = real
            .parent()
            .ok_or_else(|| FsError::Io(format!("{path} has no parent directory")))?;
        std::fs::create_dir_all(parent).map_err(|error| io_error(path, error))?;
        // Write-then-rename in the same directory: atomic on POSIX, so an
        // agent or editor reading concurrently sees either the old or the
        // new content, never a truncated file. The original file's
        // permissions are carried over so saving never silently drops an
        // executable bit.
        let temp = parent.join(format!(
            ".{}.duet-{}.tmp",
            path.file_name(),
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(contents)?;
            file.sync_all()?;
            if let Ok(existing) = std::fs::metadata(&real) {
                std::fs::set_permissions(&temp, existing.permissions())?;
            }
            std::fs::rename(&temp, &real)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(io_error(path, error));
        }
        Ok(())
    }
}

impl ProjectCommands for LocalProject {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> std::io::Result<CommandOutput> {
        use std::process::{Command, Stdio};
        let mut command = Command::new(program);
        if program == "git" {
            // Never block on a credential prompt: there is no terminal to
            // answer it, so a fetch/push that needs one fails with git's
            // own message instead of hanging.
            command.env("GIT_TERMINAL_PROMPT", "0");
        }
        let mut child = command
            .args(args)
            .current_dir(&self.root)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            // Written from a separate thread: a tool that produces output
            // while still reading (git check-ignore does) would otherwise
            // deadlock against a full stdout pipe.
            let input = input.to_vec();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            });
        }
        let output = child.wait_with_output()?;
        Ok(CommandOutput {
            status: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// What kind of content a file holds, for deciding how to open it: a
/// Markdown file becomes a file-backed Note, other text opens in the
/// editor, an image is shown as an image, anything else is reported as
/// unsupported rather than rendered as garbage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileClass {
    Markdown,
    Text,
    Image,
    Binary,
}

const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd"];
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg"];

/// Whether `bytes` (a file's leading bytes) look like text: valid UTF-8 —
/// tolerating a multi-byte character cut off at the end of the sample — and
/// no NUL bytes.
pub fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return false;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none() && bytes.len() - error.valid_up_to() < 4,
    }
}

/// Classifies a file from its name and leading bytes (`head`, typically the
/// first few KiB).
pub fn classify(path: &ProjectPath, head: &[u8]) -> FileClass {
    let extension = path.extension();
    if extension
        .as_deref()
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext))
    {
        return FileClass::Image;
    }
    if !looks_like_text(head) {
        return FileClass::Binary;
    }
    if extension
        .as_deref()
        .is_some_and(|ext| MARKDOWN_EXTENSIONS.contains(&ext))
    {
        FileClass::Markdown
    } else {
        FileClass::Text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn p(raw: &str) -> ProjectPath {
        ProjectPath::parse(raw).unwrap()
    }

    #[test]
    fn write_read_metadata_and_list_round_trip() {
        let tmp = tempdir().unwrap();
        let project = LocalProject::new(tmp.path());
        project
            .write(&p("src/auth.rs"), b"fn login() {}\n")
            .unwrap();
        project.write(&p("README.md"), b"# Hi\n").unwrap();
        project.write(&p(".hidden"), b"x").unwrap();

        assert_eq!(project.read(&p("src/auth.rs")).unwrap(), b"fn login() {}\n");
        let metadata = project.metadata(&p("src/auth.rs")).unwrap();
        assert_eq!(metadata.kind, EntryKind::File);
        assert_eq!(metadata.size, 14);
        assert!(metadata.modified.is_some());
        assert!(project.exists(&p("src")));
        assert!(!project.exists(&p("nope.rs")));

        let names: Vec<String> = project
            .list_dir(&ProjectPath::root())
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        // Directories first, then case-insensitive name order.
        assert_eq!(names, vec!["src", ".hidden", "README.md"]);
        let src = project.list_dir(&p("src")).unwrap();
        assert_eq!(src[0].path.as_str(), "src/auth.rs");
    }

    #[test]
    fn write_is_atomic_and_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().unwrap();
        let project = LocalProject::new(tmp.path());
        project.write(&p("run.sh"), b"#!/bin/sh\n").unwrap();
        let real = tmp.path().join("run.sh");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        project
            .write(&p("run.sh"), b"#!/bin/sh\necho hi\n")
            .unwrap();
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        // No temp files are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("duet-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn errors_are_specific() {
        let tmp = tempdir().unwrap();
        let project = LocalProject::new(tmp.path());
        project.write(&p("a/b.txt"), b"x").unwrap();
        assert_eq!(
            project.read(&p("missing.txt")),
            Err(FsError::NotFound(p("missing.txt")))
        );
        assert_eq!(project.read(&p("a")), Err(FsError::IsADirectory(p("a"))));
        assert_eq!(
            project.list_dir(&p("a/b.txt")),
            Err(FsError::NotADirectory(p("a/b.txt")))
        );
        assert!(project.write(&ProjectPath::root(), b"x").is_err());
    }

    #[test]
    fn symlinks_escaping_the_root_are_refused() {
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        let tmp = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            tmp.path().join("secret-link"),
        )
        .unwrap();
        let project = LocalProject::new(tmp.path());

        assert_eq!(
            project.read(&p("escape/secret.txt")),
            Err(FsError::OutsideRoot(p("escape/secret.txt")))
        );
        assert_eq!(
            project.read(&p("secret-link")),
            Err(FsError::OutsideRoot(p("secret-link")))
        );
        assert!(project.write(&p("escape/new.txt"), b"x").is_err());
        assert!(!outside.path().join("new.txt").exists());
        // Still listed (so the user sees it exists), but as a bare symlink.
        let entries = project.list_dir(&ProjectPath::root()).unwrap();
        assert!(entries.iter().all(|entry| entry.kind == EntryKind::Symlink));
    }

    #[test]
    fn symlinks_inside_the_root_are_followed() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        std::fs::write(tmp.path().join("real/a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("alias")).unwrap();
        let project = LocalProject::new(tmp.path());
        assert_eq!(project.read(&p("alias/a.txt")).unwrap(), b"a");
        let alias = project
            .list_dir(&ProjectPath::root())
            .unwrap()
            .into_iter()
            .find(|entry| entry.name == "alias")
            .unwrap();
        assert_eq!(alias.kind, EntryKind::Directory);
    }

    #[test]
    fn run_executes_in_the_project_root_without_a_shell() {
        let tmp = tempdir().unwrap();
        let project = LocalProject::new(tmp.path());
        let output = project.run("pwd", &[], None).unwrap();
        assert!(output.success());
        let printed = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            Path::new(printed.trim()).canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
        // An argument containing shell metacharacters is passed literally.
        let echo = project.run("echo", &["$(whoami); rm -rf /"], None).unwrap();
        assert_eq!(
            String::from_utf8(echo.stdout).unwrap(),
            "$(whoami); rm -rf /\n"
        );
        let cat = project.run("cat", &[], Some(b"piped")).unwrap();
        assert_eq!(cat.stdout, b"piped");
        assert!(
            project
                .run("definitely-not-a-real-program-duet", &[], None)
                .is_err()
        );
    }

    #[test]
    fn classify_distinguishes_markdown_text_image_and_binary() {
        assert_eq!(classify(&p("README.md"), b"# hi"), FileClass::Markdown);
        assert_eq!(classify(&p("src/a.rs"), b"fn a() {}"), FileClass::Text);
        assert_eq!(classify(&p("logo.PNG"), b"\x89PNG\0\0"), FileClass::Image);
        assert_eq!(classify(&p("a.bin"), b"\0\x01\x02"), FileClass::Binary);
        assert_eq!(
            classify(&p("bad.md"), &[0xff, 0xfe, 0x00]),
            FileClass::Binary
        );
        // A multi-byte character cut off at the end of the sample is still text.
        let snowman = "abc☃".as_bytes();
        assert!(looks_like_text(&snowman[..snowman.len() - 1]));
    }
}
