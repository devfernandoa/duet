//! The file application service: the operations `duetctl file ...`, the
//! embedded editor, the FileTree and file-backed notes all share — inspect,
//! read (optionally a line range), checked write, and the "how should this
//! file open" decision. GTK and the CLI both call these; neither
//! reimplements them (CLAUDE.md, "CLI and GUI").

use super::fs::{EntryKind, FileClass, Project, classify};
use super::git::GitService;
use super::path::{LineRange, ProjectPath};
use super::sync::{FileRevision, WriteError, write_checked};
use serde::{Deserialize, Serialize};

/// Text files larger than this aren't returned whole by `read_text`
/// (unless a line range narrows them) — an agent asking for a 200 MB log
/// would otherwise flood its own context and the control socket.
pub const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// How many leading bytes `classify` looks at.
const SNIFF_BYTES: usize = 8192;

/// `duetctl file inspect`'s answer: identity and metadata, never content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: ProjectPath,
    pub reference: String,
    pub kind: EntryKind,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<FileClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<FileRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_count: Option<usize>,
    /// The file's Git status marker (`M`, `A`, `?`, ...), if it has changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_status: Option<char>,
}

/// `duetctl file read`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    pub path: ProjectPath,
    pub reference: String,
    /// The revision of the *whole* file the content was read from — what a
    /// later `file write --revision` must quote back.
    pub revision: FileRevision,
    pub total_lines: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    pub content: String,
}

pub fn inspect(project: &dyn Project, path: &ProjectPath) -> Result<FileInfo, String> {
    let metadata = project.metadata(path).map_err(|error| error.to_string())?;
    let git_status = GitService::new(project)
        .status()
        .ok()
        .and_then(|status| status.entry(path).map(|entry| entry.marker()));
    let mut info = FileInfo {
        path: path.clone(),
        reference: super::path::file_reference(path, None),
        kind: metadata.kind,
        size: metadata.size,
        modified: metadata.modified,
        class: None,
        revision: None,
        line_count: None,
        git_status,
    };
    if metadata.kind == EntryKind::File && metadata.size <= MAX_READ_BYTES {
        let bytes = project.read(path).map_err(|error| error.to_string())?;
        let class = classify(path, &bytes[..bytes.len().min(SNIFF_BYTES)]);
        info.revision = Some(FileRevision::of(&bytes));
        if matches!(class, FileClass::Markdown | FileClass::Text) {
            info.line_count = Some(String::from_utf8_lossy(&bytes).lines().count());
        }
        info.class = Some(class);
    }
    Ok(info)
}

/// How a file should be opened on the canvas.
pub fn classify_file(project: &dyn Project, path: &ProjectPath) -> Result<FileClass, String> {
    let metadata = project.metadata(path).map_err(|error| error.to_string())?;
    if metadata.kind == EntryKind::Directory {
        return Err(format!("{path} is a directory"));
    }
    let bytes = project.read(path).map_err(|error| error.to_string())?;
    Ok(classify(path, &bytes[..bytes.len().min(SNIFF_BYTES)]))
}

/// Reads a text file, or just `lines` of it. Refuses directories, binary
/// files, and (without a range) files over `MAX_READ_BYTES`.
pub fn read_text(
    project: &dyn Project,
    path: &ProjectPath,
    lines: Option<LineRange>,
) -> Result<FileContent, String> {
    let metadata = project.metadata(path).map_err(|error| error.to_string())?;
    if metadata.kind == EntryKind::Directory {
        return Err(format!("{path} is a directory; use `duetctl file list`"));
    }
    if lines.is_none() && metadata.size > MAX_READ_BYTES {
        return Err(format!(
            "{path} is {} bytes; read a line range instead (e.g. {}#L1-200)",
            metadata.size,
            super::path::file_reference(path, None)
        ));
    }
    let bytes = project.read(path).map_err(|error| error.to_string())?;
    if matches!(
        classify(path, &bytes[..bytes.len().min(SNIFF_BYTES)]),
        FileClass::Binary | FileClass::Image
    ) {
        return Err(format!("{path} is not a text file"));
    }
    let revision = FileRevision::of(&bytes);
    let text = String::from_utf8_lossy(&bytes).to_string();
    let total_lines = text.lines().count();
    let content = match lines {
        None => text,
        Some(range) => {
            if range.start as usize > total_lines.max(1) {
                return Err(format!("{path} has only {total_lines} lines"));
            }
            text.split_inclusive('\n')
                .skip(range.start as usize - 1)
                .take((range.end - range.start + 1) as usize)
                .collect()
        }
    };
    Ok(FileContent {
        path: path.clone(),
        reference: super::path::file_reference(path, lines),
        revision,
        total_lines,
        lines,
        content,
    })
}

/// Writes text through the optimistic-concurrency check (`sync::
/// write_checked`): `expected` is the revision the caller's edit is based
/// on, `None` to create a file that must not exist yet.
pub fn write_text(
    project: &dyn Project,
    path: &ProjectPath,
    expected: Option<&FileRevision>,
    content: &str,
) -> Result<FileRevision, WriteError> {
    write_checked(project, path, expected, content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::fs::{LocalProject, ProjectFilesystem};
    use crate::project::git::tests::{p, repo};
    use tempfile::tempdir;

    #[test]
    fn inspect_reports_metadata_class_revision_and_git_status() {
        let (_tmp, project) = repo();
        project.write(&p("src/auth.rs"), b"a\nb\nc\n").unwrap();
        let info = inspect(&project, &p("src/auth.rs")).unwrap();
        assert_eq!(info.kind, EntryKind::File);
        assert_eq!(info.class, Some(FileClass::Text));
        assert_eq!(info.line_count, Some(3));
        assert_eq!(info.git_status, Some('M'));
        assert_eq!(info.reference, "@file:src/auth.rs");
        assert_eq!(info.revision, Some(FileRevision::of(b"a\nb\nc\n")));
        let dir = inspect(&project, &p("src")).unwrap();
        assert_eq!(dir.kind, EntryKind::Directory);
        assert_eq!(dir.revision, None);
        assert!(inspect(&project, &p("nope")).is_err());
    }

    #[test]
    fn read_text_returns_whole_files_and_line_ranges() {
        let tmp = tempdir().unwrap();
        let project = LocalProject::new(tmp.path());
        project
            .write(&p("a.txt"), b"one\ntwo\nthree\nfour\n")
            .unwrap();
        let whole = read_text(&project, &p("a.txt"), None).unwrap();
        assert_eq!(whole.content, "one\ntwo\nthree\nfour\n");
        assert_eq!(whole.total_lines, 4);
        let slice = read_text(&project, &p("a.txt"), Some(LineRange { start: 2, end: 3 })).unwrap();
        assert_eq!(slice.content, "two\nthree\n");
        assert_eq!(slice.reference, "@file:a.txt#L2-3");
        assert_eq!(slice.revision, whole.revision);
        // A range running past the end is clipped, not an error.
        let tail = read_text(&project, &p("a.txt"), Some(LineRange { start: 4, end: 99 })).unwrap();
        assert_eq!(tail.content, "four\n");
        assert!(read_text(&project, &p("a.txt"), Some(LineRange { start: 9, end: 9 })).is_err());

        project.write(&p("blob.bin"), &[0, 1, 2]).unwrap();
        assert!(read_text(&project, &p("blob.bin"), None).is_err());
        assert!(read_text(&project, &ProjectPath::root(), None).is_err());
    }

    #[test]
    fn write_text_uses_the_revision_check() {
        let tmp = tempdir().unwrap();
        let project = LocalProject::new(tmp.path());
        let created = write_text(&project, &p("n.md"), None, "# one").unwrap();
        assert!(write_text(&project, &p("n.md"), None, "# clobber").is_err());
        write_text(&project, &p("n.md"), Some(&created), "# two").unwrap();
        assert_eq!(project.read(&p("n.md")).unwrap(), b"# two");
    }

    #[test]
    fn classify_file_refuses_directories() {
        let tmp = tempdir().unwrap();
        let project = LocalProject::new(tmp.path());
        project.write(&p("d/x.md"), b"# x").unwrap();
        assert_eq!(
            classify_file(&project, &p("d/x.md")).unwrap(),
            FileClass::Markdown
        );
        assert!(classify_file(&project, &p("d")).is_err());
    }
}
