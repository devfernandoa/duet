//! Git, behind a service boundary: status, diffs, log, stage/unstage,
//! discard and commit, all by running the `git` CLI through
//! [`ProjectCommands`] — so the same code runs against a remote project once
//! Milestone 13's SSH/Docker hosts implement that trait, and no GTK callback
//! ever shells out to git itself (the FileTree, the editor and `duetctl git`
//! all call this).
//!
//! Every invocation passes `--literal-pathspecs`, so a file literally named
//! `*.rs` is never treated as a glob, and every path is a [`ProjectPath`]
//! relative to the project root (git's own working directory here), so a
//! pathspec can't reach outside the project either.
//!
//! A project root may be a subdirectory of a larger repository; git reports
//! status paths relative to the repository top level, so [`GitService::status`]
//! strips the project's own prefix (`git rev-parse --show-prefix`) and drops
//! anything outside it.

use super::fs::{CommandOutput, ProjectCommands};
use super::path::ProjectPath;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// `git` itself couldn't be started.
    Unavailable(String),
    NotARepository,
    /// A destructive action was requested without explicit confirmation.
    ConfirmationRequired(String),
    Invalid(String),
    Failed(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Unavailable(message) => write!(f, "git is not available: {message}"),
            GitError::NotARepository => {
                write!(f, "the project root is not inside a Git repository")
            }
            GitError::ConfirmationRequired(message) => write!(f, "{message}"),
            GitError::Invalid(message) => write!(f, "{message}"),
            GitError::Failed(message) => write!(f, "git failed: {message}"),
        }
    }
}

impl std::error::Error for GitError {}

/// One changed path, from `git status --porcelain=v1`. `index`/`worktree`
/// are git's own two status letters (`' '` meaning unchanged on that side).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatusEntry {
    pub path: ProjectPath,
    /// For a rename/copy, where it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_path: Option<ProjectPath>,
    pub index: char,
    pub worktree: char,
}

impl GitStatusEntry {
    pub fn is_untracked(&self) -> bool {
        self.index == '?' && self.worktree == '?'
    }

    pub fn is_conflicted(&self) -> bool {
        matches!(
            (self.index, self.worktree),
            ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D')
        )
    }

    /// Whether something about this path is staged.
    pub fn has_staged(&self) -> bool {
        !self.is_untracked() && !self.is_conflicted() && self.index != ' '
    }

    /// Whether the working tree differs from the index for this path.
    pub fn has_unstaged(&self) -> bool {
        self.is_untracked() || (!self.is_conflicted() && self.worktree != ' ')
    }

    /// The single-letter marker the FileTree shows: `U` for a conflict,
    /// `?` untracked, otherwise the worktree letter if the working tree
    /// differs, else the index letter (`M`/`A`/`D`/`R`/...).
    pub fn marker(&self) -> char {
        if self.is_conflicted() {
            'U'
        } else if self.is_untracked() {
            '?'
        } else if self.worktree != ' ' {
            self.worktree
        } else {
            self.index
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    /// The current branch, `None` when detached.
    pub branch: Option<String>,
    pub entries: Vec<GitStatusEntry>,
}

impl GitStatus {
    pub fn entry(&self, path: &ProjectPath) -> Option<&GitStatusEntry> {
        self.entries.iter().find(|entry| &entry.path == path)
    }

    /// Whether anything at or below `dir` has changed — for marking a
    /// collapsed directory in the FileTree.
    pub fn has_changes_under(&self, dir: &ProjectPath) -> bool {
        self.entries.iter().any(|entry| entry.path.starts_with(dir))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLogEntry {
    pub hash: String,
    pub short_hash: String,
    pub author: String,
    /// Unix epoch seconds.
    pub timestamp: u64,
    pub subject: String,
}

/// Which two states a diff compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiffScope {
    /// Working tree vs index (`git diff`) — what `stage` would add.
    #[default]
    Unstaged,
    /// Index vs `HEAD` (`git diff --cached`) — what `commit` would record.
    Staged,
    /// Working tree vs `HEAD`: everything not yet committed. What an
    /// `@diff:` reference means.
    Head,
}

/// The empty tree's well-known object id: what `HEAD` is compared against
/// in a repository with no commits yet.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Untracked files appended to a whole-project `Head` diff, at most — a
/// fresh checkout of a generated directory shouldn't produce a megabyte diff.
const MAX_UNTRACKED_DIFFS: usize = 50;

pub struct GitService<'a> {
    project: &'a dyn ProjectCommands,
}

impl<'a> GitService<'a> {
    pub fn new(project: &'a dyn ProjectCommands) -> GitService<'a> {
        GitService { project }
    }

    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<CommandOutput, GitError> {
        // `check-ignore` takes paths, not pathspecs, and refuses the
        // `--literal-pathspecs` magic outright.
        let literal = args.first() != Some(&"check-ignore");
        let mut full = if literal {
            vec!["--literal-pathspecs", "-c", "core.quotepath=false"]
        } else {
            vec!["-c", "core.quotepath=false"]
        };
        full.extend_from_slice(args);
        self.project
            .run("git", &full, stdin)
            .map_err(|error| GitError::Unavailable(error.to_string()))
    }

    fn run_ok(&self, args: &[&str]) -> Result<CommandOutput, GitError> {
        let output = self.run(args, None)?;
        if output.success() {
            Ok(output)
        } else {
            let stderr = output.stderr_text();
            if stderr.contains("not a git repository") {
                Err(GitError::NotARepository)
            } else {
                Err(GitError::Failed(stderr))
            }
        }
    }

    pub fn is_repository(&self) -> bool {
        self.run(&["rev-parse", "--is-inside-work-tree"], None)
            .is_ok_and(|output| output.success() && output.stdout.starts_with(b"true"))
    }

    /// The project root's own path inside the repository (`""` when the
    /// project root is the repository root, `"sub/dir/"` otherwise).
    fn prefix(&self) -> Result<String, GitError> {
        let output = self.run_ok(&["rev-parse", "--show-prefix"])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn has_head(&self) -> bool {
        self.run(&["rev-parse", "--verify", "--quiet", "HEAD"], None)
            .is_ok_and(|output| output.success())
    }

    pub fn status(&self) -> Result<GitStatus, GitError> {
        let prefix = self.prefix()?;
        let output = self.run_ok(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--branch",
            "--untracked-files=all",
        ])?;
        Ok(parse_status(&output.stdout, &prefix))
    }

    pub fn diff(&self, path: Option<&ProjectPath>, scope: DiffScope) -> Result<String, GitError> {
        let pathspec = path.filter(|path| !path.is_root()).map(|p| p.as_str());
        let mut args: Vec<&str> = vec!["diff", "--no-color", "--no-ext-diff"];
        match scope {
            DiffScope::Unstaged => {}
            DiffScope::Staged => args.push("--cached"),
            DiffScope::Head => args.push(if self.has_head() { "HEAD" } else { EMPTY_TREE }),
        }
        args.push("--");
        if let Some(pathspec) = pathspec {
            args.push(pathspec);
        }
        let mut diff = String::from_utf8_lossy(&self.run_ok(&args)?.stdout).to_string();

        // `git diff` never shows untracked files; for anything but the
        // staged view, an untracked file's "diff" is its whole content, the
        // same thing `git add` would stage.
        if scope != DiffScope::Staged {
            let status = self.status()?;
            let untracked = status
                .entries
                .iter()
                .filter(|entry| entry.is_untracked())
                .filter(|entry| path.is_none_or(|path| entry.path.starts_with(path)))
                .take(MAX_UNTRACKED_DIFFS);
            for entry in untracked {
                let output = self.run(
                    &[
                        "diff",
                        "--no-color",
                        "--no-ext-diff",
                        "--no-index",
                        "--",
                        "/dev/null",
                        entry.path.as_str(),
                    ],
                    None,
                )?;
                // `--no-index` exits 1 when the files differ, which they do.
                diff.push_str(&String::from_utf8_lossy(&output.stdout));
            }
        }
        Ok(diff)
    }

    pub fn log(
        &self,
        path: Option<&ProjectPath>,
        limit: usize,
    ) -> Result<Vec<GitLogEntry>, GitError> {
        if !self.has_head() {
            // A brand-new repository has no history; that's not an error.
            self.prefix()?;
            return Ok(Vec::new());
        }
        let limit = limit.max(1).to_string();
        let mut args = vec![
            "log",
            "-n",
            limit.as_str(),
            "--format=%H%x1f%h%x1f%an%x1f%at%x1f%s%x1e",
        ];
        if let Some(path) = path.filter(|path| !path.is_root()) {
            args.push("--");
            args.push(path.as_str());
        }
        let output = self.run_ok(&args)?;
        Ok(parse_log(&output.stdout))
    }

    fn require_paths(paths: &[ProjectPath]) -> Result<Vec<&str>, GitError> {
        if paths.is_empty() {
            return Err(GitError::Invalid("no paths given".to_string()));
        }
        Ok(paths
            .iter()
            .map(|path| if path.is_root() { "." } else { path.as_str() })
            .collect())
    }

    /// `git add -A -- <paths>`: stages modifications, additions and
    /// deletions alike.
    pub fn stage(&self, paths: &[ProjectPath]) -> Result<(), GitError> {
        let mut args = vec!["add", "-A", "--"];
        args.extend(Self::require_paths(paths)?);
        self.run_ok(&args).map(|_| ())
    }

    /// Removes paths from the index without touching the working tree.
    pub fn unstage(&self, paths: &[ProjectPath]) -> Result<(), GitError> {
        let specs = Self::require_paths(paths)?;
        let mut args = if self.has_head() {
            vec!["restore", "--staged", "--"]
        } else {
            vec!["rm", "--cached", "-r", "-q", "--"]
        };
        args.extend(specs);
        self.run_ok(&args).map(|_| ())
    }

    /// Discards unstaged working-tree changes to tracked files (`git
    /// restore --worktree`). Destructive, so it refuses unless `confirmed`;
    /// it also refuses untracked files outright — "discarding" one would mean
    /// deleting it, which deserves its own explicit action, not a side
    /// effect of this one.
    pub fn discard(&self, paths: &[ProjectPath], confirmed: bool) -> Result<(), GitError> {
        let specs = Self::require_paths(paths)?;
        if !confirmed {
            return Err(GitError::ConfirmationRequired(format!(
                "discarding changes to {} permanently loses them; confirm explicitly",
                specs.join(", ")
            )));
        }
        let status = self.status()?;
        if let Some(untracked) = status
            .entries
            .iter()
            .find(|entry| entry.is_untracked() && paths.contains(&entry.path))
        {
            return Err(GitError::Invalid(format!(
                "{} is untracked; there is no committed version to restore",
                untracked.path
            )));
        }
        let mut args = vec!["restore", "--worktree", "--"];
        args.extend(specs);
        self.run_ok(&args).map(|_| ())
    }

    /// Commits whatever is staged and returns the new commit's short hash.
    pub fn commit(&self, message: &str) -> Result<String, GitError> {
        if message.trim().is_empty() {
            return Err(GitError::Invalid("a commit needs a message".to_string()));
        }
        let status = self.status()?;
        if !status.entries.iter().any(GitStatusEntry::has_staged) {
            return Err(GitError::Invalid(
                "nothing is staged; stage changes before committing".to_string(),
            ));
        }
        self.run_ok(&["commit", "--quiet", "-m", message])?;
        let output = self.run_ok(&["rev-parse", "--short", "HEAD"])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Which of `paths` git ignores (`.gitignore`, `.git/info/exclude`, the
    /// user's global excludes). Not a repository, or git missing, means
    /// nothing is ignored — `.gitignore` support degrades gracefully rather
    /// than hiding files or failing the listing.
    pub fn ignored(&self, paths: &[ProjectPath]) -> Vec<ProjectPath> {
        if paths.is_empty() {
            return Vec::new();
        }
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_str().as_bytes());
            input.push(0);
        }
        let Ok(output) = self.run(&["check-ignore", "-z", "--stdin"], Some(&input)) else {
            return Vec::new();
        };
        // 0: some ignored, 1: none ignored, anything else: not a repo/error.
        if output.status != Some(0) {
            return Vec::new();
        }
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|chunk| !chunk.is_empty())
            .filter_map(|chunk| ProjectPath::parse(&String::from_utf8_lossy(chunk)).ok())
            .collect()
    }
}

/// Parses `git status --porcelain=v1 -z --branch` output. Paths are
/// reported relative to the repository top level; `prefix` (the project
/// root's own path inside the repository, with a trailing `/`) is stripped,
/// and entries outside it are dropped.
pub fn parse_status(raw: &[u8], prefix: &str) -> GitStatus {
    let mut branch = None;
    let mut entries = Vec::new();
    let mut records = raw.split(|byte| *byte == 0).filter(|r| !r.is_empty());
    let relative = |repo_path: &str| -> Option<ProjectPath> {
        ProjectPath::parse(repo_path.strip_prefix(prefix)?).ok()
    };
    while let Some(record) = records.next() {
        let text = String::from_utf8_lossy(record);
        if let Some(header) = text.strip_prefix("## ") {
            branch = parse_branch_header(header);
            continue;
        }
        let mut chars = text.chars();
        let (Some(index), Some(worktree), Some(' ')) = (chars.next(), chars.next(), chars.next())
        else {
            continue;
        };
        let repo_path: String = chars.collect();
        // Renames and copies carry their original path as the next record.
        let original = if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            records
                .next()
                .map(|r| String::from_utf8_lossy(r).to_string())
        } else {
            None
        };
        let Some(path) = relative(&repo_path) else {
            continue;
        };
        entries.push(GitStatusEntry {
            path,
            original_path: original.as_deref().and_then(relative),
            index,
            worktree,
        });
    }
    GitStatus { branch, entries }
}

fn parse_branch_header(header: &str) -> Option<String> {
    if header.starts_with("HEAD (no branch)") {
        return None;
    }
    let name = header.strip_prefix("No commits yet on ").unwrap_or(header);
    let name = name.split("...").next().unwrap_or(name);
    let name = name.split(' ').next().unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

pub fn parse_log(raw: &[u8]) -> Vec<GitLogEntry> {
    String::from_utf8_lossy(raw)
        .split('\x1e')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            let mut fields = record.split('\x1f');
            let hash = fields.next()?.to_string();
            if hash.is_empty() {
                return None;
            }
            Some(GitLogEntry {
                hash,
                short_hash: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                timestamp: fields.next()?.parse().unwrap_or(0),
                subject: fields.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::project::fs::{LocalProject, ProjectFilesystem};
    use tempfile::{TempDir, tempdir};

    pub(crate) fn p(raw: &str) -> ProjectPath {
        ProjectPath::parse(raw).unwrap()
    }

    pub(crate) fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(status.status.success(), "git {args:?}: {status:?}");
    }

    /// A temporary repository with one committed file, `src/auth.rs`, and a
    /// local identity so commits work on any machine.
    pub(crate) fn repo() -> (TempDir, LocalProject) {
        let tmp = tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.name", "Duet Test"]);
        git(
            tmp.path(),
            &["config", "user.email", "duet@example.invalid"],
        );
        git(tmp.path(), &["config", "commit.gpgsign", "false"]);
        let project = LocalProject::new(tmp.path());
        project
            .write(&p("src/auth.rs"), b"fn login() {}\n")
            .unwrap();
        project
            .write(&p(".gitignore"), b"target/\n*.log\n")
            .unwrap();
        git(tmp.path(), &["add", "-A"]);
        git(tmp.path(), &["commit", "-q", "-m", "initial"]);
        (tmp, project)
    }

    #[test]
    fn parse_status_handles_renames_prefixes_and_branches() {
        let raw = b"## main...origin/main [ahead 1]\0 M sub/a.rs\0R  sub/new.rs\0sub/old.rs\0?? other/x.rs\0A  sub/b.rs\0";
        let status = parse_status(raw, "sub/");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.entries.len(), 3);
        assert_eq!(status.entries[0].path, p("a.rs"));
        assert_eq!(status.entries[0].marker(), 'M');
        assert_eq!(status.entries[1].path, p("new.rs"));
        assert_eq!(status.entries[1].original_path, Some(p("old.rs")));
        assert_eq!(status.entries[1].marker(), 'R');
        assert!(status.entries[1].has_staged());
        assert_eq!(status.entries[2].marker(), 'A');
        assert_eq!(
            parse_branch_header("No commits yet on main").as_deref(),
            Some("main")
        );
        assert_eq!(parse_branch_header("HEAD (no branch)"), None);
    }

    #[test]
    fn status_diff_stage_unstage_commit_and_log() {
        let (_tmp, project) = repo();
        let git = GitService::new(&project);
        assert!(git.is_repository());
        assert_eq!(git.status().unwrap().entries, vec![]);
        assert_eq!(git.status().unwrap().branch.as_deref(), Some("main"));

        project
            .write(&p("src/auth.rs"), b"fn login() { check() }\n")
            .unwrap();
        project.write(&p("src/new.rs"), b"new\n").unwrap();
        project.write(&p("debug.log"), b"ignored\n").unwrap();

        let status = git.status().unwrap();
        let markers: Vec<(String, char)> = status
            .entries
            .iter()
            .map(|e| (e.path.to_string(), e.marker()))
            .collect();
        assert_eq!(
            markers,
            vec![
                ("src/auth.rs".to_string(), 'M'),
                ("src/new.rs".to_string(), '?')
            ]
        );
        assert!(status.has_changes_under(&p("src")));

        let unstaged = git
            .diff(Some(&p("src/auth.rs")), DiffScope::Unstaged)
            .unwrap();
        assert!(unstaged.contains("-fn login() {}"));
        assert!(unstaged.contains("+fn login() { check() }"));
        // An untracked file's diff is its whole content.
        let untracked = git
            .diff(Some(&p("src/new.rs")), DiffScope::Unstaged)
            .unwrap();
        assert!(untracked.contains("+new"));
        assert_eq!(git.diff(None, DiffScope::Staged).unwrap(), "");

        git.stage(&[p("src/auth.rs")]).unwrap();
        let staged = git.diff(None, DiffScope::Staged).unwrap();
        assert!(staged.contains("+fn login() { check() }"));
        assert!(
            git.status()
                .unwrap()
                .entry(&p("src/auth.rs"))
                .unwrap()
                .has_staged()
        );
        let head = git.diff(None, DiffScope::Head).unwrap();
        assert!(head.contains("src/auth.rs") && head.contains("src/new.rs"));

        git.unstage(&[p("src/auth.rs")]).unwrap();
        assert!(
            !git.status()
                .unwrap()
                .entry(&p("src/auth.rs"))
                .unwrap()
                .has_staged()
        );

        assert!(matches!(
            git.commit("nothing staged"),
            Err(GitError::Invalid(_))
        ));
        git.stage(&[p("src")]).unwrap();
        assert!(matches!(git.commit("  "), Err(GitError::Invalid(_))));
        let hash = git.commit("feat: check login").unwrap();
        assert!(!hash.is_empty());

        let log = git.log(None, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "feat: check login");
        assert_eq!(log[0].short_hash, hash);
        assert_eq!(log[0].author, "Duet Test");
        assert_eq!(git.log(Some(&p("src/new.rs")), 10).unwrap().len(), 1);
    }

    #[test]
    fn discard_requires_confirmation_and_refuses_untracked_files() {
        let (_tmp, project) = repo();
        let git = GitService::new(&project);
        project.write(&p("src/auth.rs"), b"broken\n").unwrap();
        project.write(&p("scratch.rs"), b"x\n").unwrap();

        assert!(matches!(
            git.discard(&[p("src/auth.rs")], false),
            Err(GitError::ConfirmationRequired(_))
        ));
        assert_eq!(project.read(&p("src/auth.rs")).unwrap(), b"broken\n");

        assert!(matches!(
            git.discard(&[p("scratch.rs")], true),
            Err(GitError::Invalid(_))
        ));
        assert!(project.exists(&p("scratch.rs")));

        git.discard(&[p("src/auth.rs")], true).unwrap();
        assert_eq!(project.read(&p("src/auth.rs")).unwrap(), b"fn login() {}\n");
    }

    #[test]
    fn ignored_follows_gitignore_and_degrades_outside_a_repository() {
        let (_tmp, project) = repo();
        project.write(&p("target/debug/app"), b"bin").unwrap();
        let git = GitService::new(&project);
        let ignored = git.ignored(&[p("target"), p("debug.log"), p("src/auth.rs")]);
        assert_eq!(ignored, vec![p("target"), p("debug.log")]);

        let plain = tempdir().unwrap();
        let plain_project = LocalProject::new(plain.path());
        let plain_git = GitService::new(&plain_project);
        assert!(!plain_git.is_repository());
        assert!(plain_git.ignored(&[p("a.log")]).is_empty());
        assert_eq!(plain_git.status(), Err(GitError::NotARepository));
    }

    #[test]
    fn a_project_root_inside_a_larger_repository_sees_only_its_own_paths() {
        let (tmp, project) = repo();
        project.write(&p("app/main.rs"), b"fn main() {}\n").unwrap();
        project.write(&p("src/auth.rs"), b"changed\n").unwrap();
        let sub = LocalProject::new(tmp.path().join("app"));
        let status = GitService::new(&sub).status().unwrap();
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].path, p("main.rs"));
    }

    #[test]
    fn a_repository_without_commits_still_reports_status_log_and_diffs() {
        let tmp = tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        let project = LocalProject::new(tmp.path());
        project.write(&p("a.txt"), b"hello\n").unwrap();
        let service = GitService::new(&project);
        assert_eq!(service.log(None, 5).unwrap(), vec![]);
        assert_eq!(service.status().unwrap().branch.as_deref(), Some("main"));
        service.stage(&[p("a.txt")]).unwrap();
        assert!(
            service
                .diff(None, DiffScope::Head)
                .unwrap()
                .contains("+hello")
        );
        service.unstage(&[p("a.txt")]).unwrap();
        assert!(service.status().unwrap().entries[0].is_untracked());
    }
}
