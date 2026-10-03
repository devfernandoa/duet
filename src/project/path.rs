//! Project-relative paths: the only path shape any `ProjectFilesystem`
//! operation, resource reference, persisted FileTree/Editor/Note payload or
//! `duetctl file` command ever carries. A [`ProjectPath`] is normalized
//! lexically at construction (`.`/empty components dropped, `..` resolved)
//! and can never name anything above its project root — escaping the root is
//! a parse error, not something each caller has to remember to check. It is
//! deliberately a plain `/`-separated string rather than a `PathBuf`: the
//! same value has to mean the same file whether the project lives on the
//! local disk today or behind SSH/Docker in Milestone 13, so it must not
//! carry any host-specific path semantics of its own.
//!
//! Lexical normalization alone can't see symlinks; `fs::LocalProject`
//! additionally canonicalizes every real path it touches and refuses one
//! that resolves outside the root (see `LocalProject::real_path`).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

/// Why a raw string isn't an acceptable [`ProjectPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// An absolute path (`/etc/passwd`) where a project-relative one was
    /// expected. `ProjectPath::from_absolute` is the explicit way in for a
    /// caller that really does hold an absolute path inside the root.
    Absolute(String),
    /// A path whose `..` components climb above the project root.
    EscapesRoot(String),
    /// An embedded NUL byte — never a valid path on any target host.
    InvalidCharacter(String),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::Absolute(raw) => write!(
                f,
                "'{raw}' is an absolute path; use a path relative to the project root"
            ),
            PathError::EscapesRoot(raw) => {
                write!(f, "'{raw}' points outside the project root")
            }
            PathError::InvalidCharacter(raw) => {
                write!(f, "'{}' contains an invalid character", raw.escape_debug())
            }
        }
    }
}

impl std::error::Error for PathError {}

/// A normalized, root-relative project path. The empty string is the
/// project root itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectPath(String);

impl TryFrom<String> for ProjectPath {
    type Error = PathError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        ProjectPath::parse(&raw)
    }
}

impl From<ProjectPath> for String {
    fn from(path: ProjectPath) -> String {
        path.0
    }
}

impl ProjectPath {
    pub fn root() -> ProjectPath {
        ProjectPath(String::new())
    }

    /// Parses and lexically normalizes a root-relative path: `./src//a.rs`
    /// and `src/x/../a.rs` both become `src/a.rs`; `.` and `` are the root.
    /// Rejects absolute paths, NUL bytes, and any `..` that would climb above
    /// the root.
    pub fn parse(raw: &str) -> Result<ProjectPath, PathError> {
        if raw.contains('\0') {
            return Err(PathError::InvalidCharacter(raw.to_string()));
        }
        if raw.starts_with('/') {
            return Err(PathError::Absolute(raw.to_string()));
        }
        let mut components: Vec<&str> = Vec::new();
        for component in raw.split('/') {
            match component {
                "" | "." => {}
                ".." => {
                    if components.pop().is_none() {
                        return Err(PathError::EscapesRoot(raw.to_string()));
                    }
                }
                other => components.push(other),
            }
        }
        Ok(ProjectPath(components.join("/")))
    }

    /// Converts an absolute host path that lies inside `root` into a
    /// `ProjectPath` — purely lexical, the same way `parse` is; callers that
    /// need symlink safety still go through `LocalProject::real_path`.
    pub fn from_absolute(root: &Path, absolute: &Path) -> Result<ProjectPath, PathError> {
        let raw = absolute.to_string_lossy().to_string();
        let relative = absolute
            .strip_prefix(root)
            .map_err(|_| PathError::EscapesRoot(raw.clone()))?;
        ProjectPath::parse(&relative.to_string_lossy())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// `self/name`, normalized — so `join("..")` can still only reach the
    /// root, never above it.
    pub fn join(&self, name: &str) -> Result<ProjectPath, PathError> {
        if name.starts_with('/') {
            return Err(PathError::Absolute(name.to_string()));
        }
        if self.is_root() {
            ProjectPath::parse(name)
        } else {
            ProjectPath::parse(&format!("{}/{name}", self.0))
        }
    }

    /// The containing directory; `None` for the root itself.
    pub fn parent(&self) -> Option<ProjectPath> {
        if self.is_root() {
            return None;
        }
        Some(match self.0.rsplit_once('/') {
            Some((parent, _)) => ProjectPath(parent.to_string()),
            None => ProjectPath::root(),
        })
    }

    /// The final component (`a.rs` for `src/a.rs`); empty for the root.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("")
    }

    /// The lowercase extension of `file_name`, without the dot. A leading
    /// dot alone (`.gitignore`) is a hidden name, not an extension.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        let (stem, extension) = name.rsplit_once('.')?;
        (!stem.is_empty() && !extension.is_empty()).then(|| extension.to_ascii_lowercase())
    }

    /// Whether `self` is `ancestor` or lies somewhere beneath it.
    pub fn starts_with(&self, ancestor: &ProjectPath) -> bool {
        ancestor.is_root()
            || self.0 == ancestor.0
            || (self.0.starts_with(&ancestor.0)
                && self.0.as_bytes().get(ancestor.0.len()) == Some(&b'/'))
    }

    /// Whether any component of this path is hidden (starts with `.`).
    pub fn is_hidden(&self) -> bool {
        self.0
            .split('/')
            .any(|component| component.starts_with('.'))
    }

    /// Number of components below the root (`src/a.rs` is 2).
    pub fn depth(&self) -> usize {
        if self.is_root() {
            0
        } else {
            self.0.split('/').count()
        }
    }

    /// The display form used in references and CLI output: `.` for the
    /// root, the path itself otherwise.
    pub fn display(&self) -> &str {
        if self.is_root() { "." } else { &self.0 }
    }
}

impl fmt::Display for ProjectPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display())
    }
}

/// An inclusive, 1-based line range — the "selected source region" half of
/// a file reference (`@file:src/auth.rs#L10-20`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    /// Parses `10`, `10-20`, `L10`, `L10-20` or `L10-L20`. Line numbers are
    /// 1-based, and `end` may not precede `start`.
    pub fn parse(raw: &str) -> Result<LineRange, String> {
        let bad = || format!("'{raw}' is not a line range (expected e.g. L10 or L10-20)");
        let strip = |part: &str| -> Result<u32, String> {
            let digits = part.strip_prefix(['L', 'l']).unwrap_or(part);
            let line: u32 = digits.parse().map_err(|_| bad())?;
            if line == 0 { Err(bad()) } else { Ok(line) }
        };
        let (start, end) = match raw.split_once('-') {
            Some((start, end)) => (strip(start)?, strip(end)?),
            None => {
                let line = strip(raw)?;
                (line, line)
            }
        };
        if end < start {
            return Err(bad());
        }
        Ok(LineRange { start, end })
    }
}

impl fmt::Display for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "L{}", self.start)
        } else {
            write!(f, "L{}-{}", self.start, self.end)
        }
    }
}

/// Splits an optional `#L10-20` selection suffix off a path-shaped
/// reference body. A `#` with anything other than a valid line range after
/// it is an error, not silently part of the file name — a typo'd selection
/// should fail loudly rather than resolve to a different (nonexistent) file.
pub fn split_line_suffix(raw: &str) -> Result<(&str, Option<LineRange>), String> {
    match raw.rsplit_once('#') {
        Some((path, lines)) => Ok((path, Some(LineRange::parse(lines)?))),
        None => Ok((raw, None)),
    }
}

/// Formats a stable, copyable file reference: `@file:src/a.rs` or
/// `@file:src/a.rs#L3-9`. The one place this syntax is produced, so the GUI's
/// "Copy reference" and the resolver's parser can't drift apart.
pub fn file_reference(path: &ProjectPath, lines: Option<LineRange>) -> String {
    match lines {
        Some(lines) => format!("@file:{}#{lines}", path.display()),
        None => format!("@file:{}", path.display()),
    }
}

/// Formats a stable diff reference: `@diff:src/a.rs` (or `@diff:.` for the
/// whole project).
pub fn diff_reference(path: &ProjectPath) -> String {
    format!("@diff:{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dot_and_empty_components() {
        assert_eq!(
            ProjectPath::parse("./src//a.rs").unwrap().as_str(),
            "src/a.rs"
        );
        assert_eq!(
            ProjectPath::parse("src/x/../a.rs").unwrap().as_str(),
            "src/a.rs"
        );
        assert!(ProjectPath::parse(".").unwrap().is_root());
        assert!(ProjectPath::parse("").unwrap().is_root());
        assert!(ProjectPath::parse("src/..").unwrap().is_root());
    }

    #[test]
    fn rejects_traversal_above_the_root() {
        assert!(matches!(
            ProjectPath::parse("../etc/passwd"),
            Err(PathError::EscapesRoot(_))
        ));
        assert!(matches!(
            ProjectPath::parse("src/../../x"),
            Err(PathError::EscapesRoot(_))
        ));
        assert!(matches!(
            ProjectPath::parse("/etc/passwd"),
            Err(PathError::Absolute(_))
        ));
        assert!(matches!(
            ProjectPath::parse("a\0b"),
            Err(PathError::InvalidCharacter(_))
        ));
    }

    #[test]
    fn join_cannot_escape_either() {
        let src = ProjectPath::parse("src").unwrap();
        assert_eq!(src.join("a.rs").unwrap().as_str(), "src/a.rs");
        assert!(src.join("..").unwrap().is_root());
        assert!(src.join("../..").is_err());
        assert!(src.join("/etc").is_err());
    }

    #[test]
    fn from_absolute_requires_the_root_prefix() {
        let root = Path::new("/home/u/project");
        assert_eq!(
            ProjectPath::from_absolute(root, Path::new("/home/u/project/src/a.rs"))
                .unwrap()
                .as_str(),
            "src/a.rs"
        );
        assert!(ProjectPath::from_absolute(root, Path::new("/home/u/other/a.rs")).is_err());
    }

    #[test]
    fn parent_file_name_extension_and_ancestry() {
        let path = ProjectPath::parse("src/auth/mod.rs").unwrap();
        assert_eq!(path.parent().unwrap().as_str(), "src/auth");
        assert_eq!(path.file_name(), "mod.rs");
        assert_eq!(path.extension().as_deref(), Some("rs"));
        assert!(path.starts_with(&ProjectPath::parse("src").unwrap()));
        assert!(!path.starts_with(&ProjectPath::parse("sr").unwrap()));
        assert!(path.starts_with(&ProjectPath::root()));
        assert_eq!(path.depth(), 3);
        assert_eq!(ProjectPath::parse(".gitignore").unwrap().extension(), None);
        assert!(ProjectPath::parse("a/.git/config").unwrap().is_hidden());
        assert_eq!(ProjectPath::root().parent(), None);
        assert_eq!(
            ProjectPath::parse("a.rs").unwrap().parent(),
            Some(ProjectPath::root())
        );
    }

    #[test]
    fn deserializing_validates_the_same_way_parsing_does() {
        let ok: ProjectPath = serde_json::from_str("\"./src/a.rs\"").unwrap();
        assert_eq!(ok.as_str(), "src/a.rs");
        assert!(serde_json::from_str::<ProjectPath>("\"../x\"").is_err());
        assert_eq!(serde_json::to_string(&ok).unwrap(), "\"src/a.rs\"");
    }

    #[test]
    fn line_ranges_parse_in_every_accepted_spelling() {
        assert_eq!(
            LineRange::parse("10").unwrap(),
            LineRange { start: 10, end: 10 }
        );
        assert_eq!(
            LineRange::parse("L10-20").unwrap(),
            LineRange { start: 10, end: 20 }
        );
        assert_eq!(
            LineRange::parse("L10-L20").unwrap(),
            LineRange { start: 10, end: 20 }
        );
        assert!(LineRange::parse("L0").is_err());
        assert!(LineRange::parse("L20-10").is_err());
        assert!(LineRange::parse("Lx").is_err());
        assert_eq!(LineRange { start: 3, end: 9 }.to_string(), "L3-9");
        assert_eq!(LineRange { start: 3, end: 3 }.to_string(), "L3");
    }

    #[test]
    fn references_round_trip_through_split_line_suffix() {
        let path = ProjectPath::parse("src/a.rs").unwrap();
        let reference = file_reference(&path, Some(LineRange { start: 2, end: 4 }));
        assert_eq!(reference, "@file:src/a.rs#L2-4");
        let body = reference.strip_prefix("@file:").unwrap();
        let (raw, lines) = split_line_suffix(body).unwrap();
        assert_eq!(raw, "src/a.rs");
        assert_eq!(lines, Some(LineRange { start: 2, end: 4 }));
        assert!(split_line_suffix("a.rs#nope").is_err());
        assert_eq!(diff_reference(&ProjectPath::root()), "@diff:.");
    }
}
