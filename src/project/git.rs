//! Git, behind a service boundary: status (with branch, upstream and
//! ahead/behind), diffs, log, stage/unstage (one path or everything),
//! discard, commit, and the everyday branch/remote workflow — list, switch,
//! create, fetch, fast-forward-only pull and push — all by running the `git`
//! CLI through
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
    /// A push of a branch with no upstream: setting one is an explicit
    /// choice, never a side effect (`push(true)` makes it).
    UpstreamRequired(PushTarget),
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
            GitError::UpstreamRequired(target) => write!(
                f,
                "{} has no upstream yet; publish it to {}/{} explicitly",
                target.branch, target.remote, target.branch
            ),
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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GitStatus {
    /// The current branch, `None` when detached.
    pub branch: Option<String>,
    /// The branch's upstream (`origin/main`), when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Commits on the branch not on its upstream, and the reverse. Both 0
    /// without an upstream (or with one that no longer exists).
    #[serde(default)]
    pub ahead: u32,
    #[serde(default)]
    pub behind: u32,
    /// The upstream is configured but its remote branch is gone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub upstream_gone: bool,
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

    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn staged_count(&self) -> usize {
        self.entries.iter().filter(|e| e.has_staged()).count()
    }

    pub fn unstaged_count(&self) -> usize {
        self.entries.iter().filter(|e| e.has_unstaged()).count()
    }

    pub fn conflict_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_conflicted()).count()
    }

    /// A one-line summary: `main ↑2 ↓1 •3` — branch (or `detached`),
    /// commits ahead/behind its upstream, and changed paths. Clean and in
    /// sync is just the branch name.
    pub fn summary(&self) -> String {
        let mut text = self
            .branch
            .clone()
            .unwrap_or_else(|| "detached".to_string());
        if self.ahead > 0 {
            text.push_str(&format!(" ↑{}", self.ahead));
        }
        if self.behind > 0 {
            text.push_str(&format!(" ↓{}", self.behind));
        }
        if !self.entries.is_empty() {
            text.push_str(&format!(" •{}", self.entries.len()));
        }
        text
    }
}

/// One local branch, from `git for-each-ref refs/heads`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBranch {
    pub name: String,
    pub current: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

/// Where a push goes when the branch has no upstream yet: `git push -u
/// <remote> <branch>`. Returned by [`GitService::push`] as
/// [`GitError::UpstreamRequired`] so the caller asks before setting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushTarget {
    pub remote: String,
    pub branch: String,
}

/// What a network operation did, in words for a toast/CLI line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSyncOutcome {
    pub message: String,
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
        // A status read (polled by the header every few seconds) must never
        // take `index.lock` — that could make a concurrent pull or switch
        // fail with "index.lock exists".
        if args.first() == Some(&"status") {
            full.insert(0, "--no-optional-locks");
        }
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

    fn head_hash(&self) -> Option<String> {
        let output = self
            .run(&["rev-parse", "--verify", "--quiet", "HEAD"], None)
            .ok()?;
        output
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
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

    /// Stages every change in the project (`git add -A -- .`, so a project
    /// that is a subdirectory of a repository stages only its own files).
    pub fn stage_all(&self) -> Result<(), GitError> {
        self.stage(&[ProjectPath::root()])
    }

    /// Unstages everything in the project, keeping the working tree.
    pub fn unstage_all(&self) -> Result<(), GitError> {
        self.unstage(&[ProjectPath::root()])
    }

    /// Every local branch, sorted by name, the current one marked.
    pub fn branches(&self) -> Result<Vec<GitBranch>, GitError> {
        let output = self.run_ok(&[
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname:short)%00%(HEAD)%00%(upstream:short)",
            "refs/heads",
        ])?;
        Ok(parse_branches(&output.stdout))
    }

    fn remotes(&self) -> Result<Vec<String>, GitError> {
        let output = self.run_ok(&["remote"])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Switches to an existing *local* branch. Never forces and never
    /// stashes: if the switch would overwrite local changes git refuses,
    /// and its own explanation is returned. `--no-guess` keeps a typo from
    /// silently creating a tracking branch off a remote one.
    pub fn switch_branch(&self, name: &str) -> Result<(), GitError> {
        let branches = self.branches()?;
        let Some(branch) = branches.iter().find(|b| b.name == name) else {
            return Err(GitError::Invalid(format!(
                "there is no local branch '{name}'"
            )));
        };
        if branch.current {
            return Ok(());
        }
        self.run_ok(&["switch", "--no-guess", "--", name])
            .map(|_| ())
            .map_err(friendly_failure)
    }

    /// Creates `name` from the current `HEAD` and switches to it (`git
    /// switch -c`). Local changes come along, as git always does.
    pub fn create_branch(&self, name: &str) -> Result<(), GitError> {
        validate_branch_name(name).map_err(GitError::Invalid)?;
        let check = self.run(&["check-ref-format", "--branch", name], None)?;
        if !check.success() {
            return Err(GitError::Invalid(format!(
                "'{name}' is not a valid branch name"
            )));
        }
        if self.branches()?.iter().any(|b| b.name == name) {
            return Err(GitError::Invalid(format!(
                "a branch named '{name}' already exists"
            )));
        }
        self.run_ok(&["switch", "-c", name])
            .map(|_| ())
            .map_err(friendly_failure)
    }

    /// `git fetch` from the branch's remote (or the only/`origin` remote).
    /// Updates remote-tracking refs only — the working tree, index and local
    /// branches are untouched, so it is always safe.
    pub fn fetch(&self) -> Result<GitSyncOutcome, GitError> {
        let remote = self.default_remote()?;
        self.run_ok(&["fetch", "--quiet", "--", &remote])
            .map_err(friendly_failure)?;
        let status = self.status()?;
        let message = match (status.upstream.as_deref(), status.behind, status.ahead) {
            (None, _, _) => format!("Fetched {remote}"),
            (Some(upstream), 0, 0) => format!("Fetched {remote}; up to date with {upstream}"),
            (Some(upstream), behind, ahead) => {
                format!("Fetched {remote}; {behind} behind and {ahead} ahead of {upstream}")
            }
        };
        Ok(GitSyncOutcome { message })
    }

    /// The conservative pull: fast-forward only, never rebase, never merge,
    /// never auto-stash. Anything else (diverged history, local changes git
    /// would overwrite, conflicts) is refused with git's own reason so the
    /// user resolves it deliberately.
    pub fn pull(&self) -> Result<GitSyncOutcome, GitError> {
        let status = self.status()?;
        let Some(branch) = status.branch.clone() else {
            return Err(GitError::Invalid(
                "HEAD is detached; switch to a branch before pulling".to_string(),
            ));
        };
        let Some(upstream) = status.upstream.clone() else {
            return Err(GitError::Invalid(format!(
                "{branch} has no upstream to pull from; push it with an upstream first"
            )));
        };
        if status.conflict_count() > 0 {
            return Err(GitError::Invalid(
                "resolve the merge conflicts before pulling".to_string(),
            ));
        }
        let head_before = self.head_hash();
        self.run_ok(&[
            "pull",
            "--ff-only",
            "--no-rebase",
            "--no-autostash",
            "--quiet",
        ])
        .map_err(|error| match error {
            GitError::Failed(message)
                if message.contains("Not possible to fast-forward")
                    || message.contains("diverg") =>
            {
                GitError::Failed(format!(
                    "{branch} and {upstream} have diverged; Duet only fast-forwards. \
                         Merge or rebase in a terminal, then pull again."
                ))
            }
            other => friendly_failure(other),
        })?;
        // Counted from HEAD itself, not the pre-pull `behind`: the pull
        // fetched first, so that number may have been stale.
        let pulled = match (head_before, self.head_hash()) {
            (Some(before), Some(after)) if before != after => self
                .run_ok(&["rev-list", "--count", &format!("{before}..{after}")])
                .ok()
                .and_then(|out| {
                    String::from_utf8_lossy(&out.stdout)
                        .trim()
                        .parse::<u32>()
                        .ok()
                })
                .unwrap_or(1),
            _ => 0,
        };
        let message = if pulled == 0 {
            format!("{branch} is already up to date with {upstream}")
        } else {
            format!(
                "Pulled {pulled} commit{} into {branch}",
                if pulled == 1 { "" } else { "s" }
            )
        };
        Ok(GitSyncOutcome { message })
    }

    /// Pushes the current branch. With an upstream, a plain `git push`;
    /// without one, refuses with [`GitError::UpstreamRequired`] unless
    /// `set_upstream` — the caller asked the user — in which case it runs
    /// `git push -u <remote> <branch>`. Never forces.
    pub fn push(&self, set_upstream: bool) -> Result<GitSyncOutcome, GitError> {
        let status = self.status()?;
        let Some(branch) = status.branch.clone() else {
            return Err(GitError::Invalid(
                "HEAD is detached; switch to a branch before pushing".to_string(),
            ));
        };
        if !self.has_head() {
            return Err(GitError::Invalid(
                "there are no commits to push yet".to_string(),
            ));
        }
        match status.upstream.clone() {
            Some(upstream) if !status.upstream_gone => {
                self.run_ok(&["push", "--quiet"])
                    .map_err(|error| match error {
                        GitError::Failed(message)
                            if message.contains("rejected")
                                || message.contains("non-fast-forward") =>
                        {
                            GitError::Failed(format!(
                                "{upstream} has commits you don't have; pull (or merge) first. \
                             Duet never force-pushes."
                            ))
                        }
                        other => friendly_failure(other),
                    })?;
                let message = if status.ahead == 0 {
                    format!("{branch} was already up to date on {upstream}")
                } else {
                    format!(
                        "Pushed {} commit{} to {upstream}",
                        status.ahead,
                        if status.ahead == 1 { "" } else { "s" }
                    )
                };
                Ok(GitSyncOutcome { message })
            }
            _ => {
                let target = PushTarget {
                    remote: self.default_remote()?,
                    branch: branch.clone(),
                };
                if !set_upstream {
                    return Err(GitError::UpstreamRequired(target));
                }
                self.run_ok(&["push", "--quiet", "-u", &target.remote, &branch])
                    .map_err(friendly_failure)?;
                Ok(GitSyncOutcome {
                    message: format!(
                        "Published {branch} to {}/{branch} and set it as upstream",
                        target.remote
                    ),
                })
            }
        }
    }

    /// The remote a fetch or a first push uses: the current branch's own
    /// remote if configured, else `origin`, else the only remote.
    fn default_remote(&self) -> Result<String, GitError> {
        let remotes = self.remotes()?;
        if remotes.is_empty() {
            return Err(GitError::Invalid(
                "this repository has no remote; add one with `git remote add`".to_string(),
            ));
        }
        if let Some(branch) = self.status()?.branch {
            let key = format!("branch.{branch}.remote");
            if let Ok(output) = self.run(&["config", "--get", &key], None)
                && output.success()
            {
                let remote = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if remotes.contains(&remote) {
                    return Ok(remote);
                }
            }
        }
        if remotes.iter().any(|r| r == "origin") {
            return Ok("origin".to_string());
        }
        if remotes.len() == 1 {
            return Ok(remotes[0].clone());
        }
        Err(GitError::Invalid(format!(
            "several remotes ({}) and none is the branch's or 'origin'",
            remotes.join(", ")
        )))
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

/// Git's stderr, minus the `hint:` lines, which talk about command-line
/// flags the user didn't type.
fn friendly_failure(error: GitError) -> GitError {
    match error {
        GitError::Failed(message) => {
            let cleaned: Vec<&str> = message
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with("hint:"))
                .collect();
            let cleaned = cleaned.join(" ");
            let cleaned = cleaned.strip_prefix("error: ").unwrap_or(&cleaned);
            let cleaned = cleaned.strip_prefix("fatal: ").unwrap_or(cleaned);
            GitError::Failed(cleaned.to_string())
        }
        other => other,
    }
}

/// Parses `git status --porcelain=v1 -z --branch` output. Paths are
/// reported relative to the repository top level; `prefix` (the project
/// root's own path inside the repository, with a trailing `/`) is stripped,
/// and entries outside it are dropped.
pub fn parse_status(raw: &[u8], prefix: &str) -> GitStatus {
    let mut header = BranchHeader::default();
    let mut entries = Vec::new();
    let mut records = raw.split(|byte| *byte == 0).filter(|r| !r.is_empty());
    let relative = |repo_path: &str| -> Option<ProjectPath> {
        ProjectPath::parse(repo_path.strip_prefix(prefix)?).ok()
    };
    while let Some(record) = records.next() {
        let text = String::from_utf8_lossy(record);
        if let Some(line) = text.strip_prefix("## ") {
            header = parse_branch_header(line);
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
    GitStatus {
        branch: header.branch,
        upstream: header.upstream,
        ahead: header.ahead,
        behind: header.behind,
        upstream_gone: header.gone,
        entries,
    }
}

/// The `## ...` line of `git status --porcelain=v1 --branch`, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchHeader {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub gone: bool,
}

/// Parses `main...origin/main [ahead 1, behind 2]`, `main`, `No commits
/// yet on main`, `HEAD (no branch)` and the `[gone]` marker.
pub fn parse_branch_header(header: &str) -> BranchHeader {
    let mut result = BranchHeader::default();
    if header.starts_with("HEAD (no branch)") {
        return result;
    }
    let header = header
        .strip_prefix("No commits yet on ")
        .or_else(|| header.strip_prefix("Initial commit on "))
        .unwrap_or(header);
    let (refs, tracking) = match header.split_once(" [") {
        Some((refs, rest)) => (refs, rest.trim_end_matches(']')),
        None => (header, ""),
    };
    let (branch, upstream) = match refs.split_once("...") {
        Some((branch, upstream)) => (branch, Some(upstream)),
        None => (refs, None),
    };
    let branch = branch.trim();
    result.branch = (!branch.is_empty()).then(|| branch.to_string());
    result.upstream = upstream
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    for part in tracking.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            result.ahead = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            result.behind = n.trim().parse().unwrap_or(0);
        } else if part.trim() == "gone" {
            result.gone = true;
        }
    }
    result
}

/// Parses `git for-each-ref --format=%(refname:short)%00%(HEAD)%00%(upstream:short)`
/// with records separated by newlines.
pub fn parse_branches(raw: &[u8]) -> Vec<GitBranch> {
    String::from_utf8_lossy(raw)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let name = fields.next()?.trim();
            if name.is_empty() {
                return None;
            }
            let current = fields.next().is_some_and(|head| head.trim() == "*");
            let upstream = fields
                .next()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_string);
            Some(GitBranch {
                name: name.to_string(),
                current,
                upstream,
            })
        })
        .collect()
}

/// Whether `name` is acceptable as a new local branch name — the rules of
/// `git check-ref-format --branch`, checked here first so the GUI can
/// validate as the user types (git still has the final word).
pub fn validate_branch_name(name: &str) -> Result<(), String> {
    let name_ref = name;
    if name_ref.is_empty() {
        return Err("a branch needs a name".to_string());
    }
    if name_ref != name_ref.trim() || name_ref.chars().any(char::is_whitespace) {
        return Err("branch names can't contain spaces".to_string());
    }
    if name_ref.starts_with('-') {
        return Err("branch names can't start with '-'".to_string());
    }
    if name_ref == "@" || name_ref == "HEAD" {
        return Err(format!("'{name_ref}' is reserved"));
    }
    if let Some(bad) = name_ref
        .chars()
        .find(|c| c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Err(format!("branch names can't contain '{bad}'"));
    }
    if name_ref.contains("..") || name_ref.contains("@{") || name_ref.contains("//") {
        return Err("branch names can't contain '..', '@{' or '//'".to_string());
    }
    if name_ref.ends_with('/') || name_ref.ends_with('.') || name_ref.ends_with(".lock") {
        return Err("branch names can't end with '/', '.' or '.lock'".to_string());
    }
    if name_ref
        .split('/')
        .any(|component| component.is_empty() || component.starts_with('.'))
    {
        return Err("no part of a branch name may be empty or start with '.'".to_string());
    }
    Ok(())
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
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (1, 0));
        assert_eq!(
            parse_branch_header("No commits yet on main")
                .branch
                .as_deref(),
            Some("main")
        );
        assert_eq!(parse_branch_header("HEAD (no branch)").branch, None);
    }

    #[test]
    fn branch_headers_parse_upstream_ahead_behind_and_gone() {
        let both = parse_branch_header("feature/x...origin/feature/x [ahead 2, behind 13]");
        assert_eq!(both.branch.as_deref(), Some("feature/x"));
        assert_eq!(both.upstream.as_deref(), Some("origin/feature/x"));
        assert_eq!((both.ahead, both.behind, both.gone), (2, 13, false));
        let behind = parse_branch_header("main...upstream/main [behind 4]");
        assert_eq!((behind.ahead, behind.behind), (0, 4));
        let gone = parse_branch_header("old...origin/old [gone]");
        assert!(gone.gone);
        assert_eq!(gone.upstream.as_deref(), Some("origin/old"));
        let plain = parse_branch_header("main");
        assert_eq!(plain.branch.as_deref(), Some("main"));
        assert_eq!(plain.upstream, None);
        assert_eq!((plain.ahead, plain.behind), (0, 0));
        let fresh = parse_branch_header("No commits yet on dev...origin/dev");
        assert_eq!(fresh.branch.as_deref(), Some("dev"));
        assert_eq!(fresh.upstream.as_deref(), Some("origin/dev"));
    }

    #[test]
    fn status_summary_reads_like_a_branch_indicator() {
        let mut status = GitStatus {
            branch: Some("main".to_string()),
            ..GitStatus::default()
        };
        assert_eq!(status.summary(), "main");
        status.ahead = 2;
        status.behind = 1;
        status.entries.push(GitStatusEntry {
            path: p("a.rs"),
            original_path: None,
            index: ' ',
            worktree: 'M',
        });
        assert_eq!(status.summary(), "main ↑2 ↓1 •1");
        status.branch = None;
        assert!(status.summary().starts_with("detached"));
    }

    #[test]
    fn branch_list_parsing_marks_the_current_branch() {
        let raw = b"feature\0 \0origin/feature\nmain\0*\0origin/main\nwip\0 \0\n";
        let branches = parse_branches(raw);
        assert_eq!(branches.len(), 3);
        assert_eq!(branches[0].name, "feature");
        assert!(!branches[0].current);
        assert_eq!(branches[0].upstream.as_deref(), Some("origin/feature"));
        assert!(branches[1].current);
        assert_eq!(branches[2].upstream, None);
    }

    #[test]
    fn branch_names_are_validated_like_git_does() {
        for good in ["feature/login", "fix-1", "release/v1.0", "a_b"] {
            assert!(validate_branch_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            " x",
            "has space",
            "-flag",
            "a..b",
            "a~b",
            "a^b",
            "a:b",
            "a?b",
            "a*b",
            "a[b",
            "a\\b",
            "end/",
            "end.",
            "x.lock",
            "a//b",
            ".hidden",
            "a/.b",
            "@",
            "HEAD",
            "a@{b",
        ] {
            assert!(
                validate_branch_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    /// A repository cloned from a local bare "remote" (no network), with an
    /// identity configured in both, plus a second clone to make upstream
    /// changes from.
    fn repo_with_remote() -> (TempDir, LocalProject, LocalProject) {
        let tmp = tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        git(
            tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                remote.to_str().unwrap(),
            ],
        );
        let seed = tmp.path().join("seed");
        std::fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        git(&seed, &["config", "user.name", "Duet Test"]);
        git(&seed, &["config", "user.email", "duet@example.invalid"]);
        git(&seed, &["config", "commit.gpgsign", "false"]);
        std::fs::write(seed.join("a.txt"), "one\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-q", "-m", "initial"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "-q", "-u", "origin", "main"]);
        let work = tmp.path().join("work");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        git(&work, &["config", "user.name", "Duet Test"]);
        git(&work, &["config", "user.email", "duet@example.invalid"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        (tmp, LocalProject::new(work), LocalProject::new(seed))
    }

    #[test]
    fn create_and_switch_branches_never_force() {
        let (_tmp, project) = repo();
        let git_service = GitService::new(&project);
        assert_eq!(
            git_service.branches().unwrap(),
            vec![GitBranch {
                name: "main".to_string(),
                current: true,
                upstream: None
            }]
        );
        assert!(matches!(
            git_service.create_branch("bad name"),
            Err(GitError::Invalid(_))
        ));
        git_service.create_branch("feature/login").unwrap();
        assert_eq!(
            git_service.status().unwrap().branch.as_deref(),
            Some("feature/login")
        );
        assert!(matches!(
            git_service.create_branch("main"),
            Err(GitError::Invalid(message)) if message.contains("already exists")
        ));
        // Commit a change on the feature branch.
        project
            .write(&p("src/auth.rs"), b"fn login() { v2() }\n")
            .unwrap();
        git_service.stage_all().unwrap();
        git_service.commit("v2").unwrap();

        // An uncommitted edit that the switch would overwrite: git refuses,
        // and the edit is still there afterwards — no force, no stash.
        project.write(&p("src/auth.rs"), b"local edit\n").unwrap();
        let refused = git_service.switch_branch("main").unwrap_err();
        assert!(
            matches!(&refused, GitError::Failed(m) if m.contains("overwritten")),
            "{refused:?}"
        );
        assert_eq!(project.read(&p("src/auth.rs")).unwrap(), b"local edit\n");
        assert_eq!(
            git_service.status().unwrap().branch.as_deref(),
            Some("feature/login")
        );

        // Unknown branches are refused rather than guessed.
        assert!(matches!(
            git_service.switch_branch("nope"),
            Err(GitError::Invalid(_))
        ));

        git_service.discard(&[p("src/auth.rs")], true).unwrap();
        git_service.switch_branch("main").unwrap();
        assert_eq!(project.read(&p("src/auth.rs")).unwrap(), b"fn login() {}\n");
        let names: Vec<_> = git_service
            .branches()
            .unwrap()
            .into_iter()
            .map(|b| (b.name, b.current))
            .collect();
        assert_eq!(
            names,
            vec![
                ("feature/login".to_string(), false),
                ("main".to_string(), true)
            ]
        );
    }

    #[test]
    fn stage_all_and_unstage_all_cover_the_whole_project() {
        let (_tmp, project) = repo();
        let git_service = GitService::new(&project);
        project.write(&p("src/auth.rs"), b"changed\n").unwrap();
        project.write(&p("docs/new.md"), b"new\n").unwrap();
        git_service.stage_all().unwrap();
        let status = git_service.status().unwrap();
        assert_eq!(status.staged_count(), 2);
        assert_eq!(status.unstaged_count(), 0);
        git_service.unstage_all().unwrap();
        let status = git_service.status().unwrap();
        assert_eq!(status.staged_count(), 0);
        assert_eq!(status.unstaged_count(), 2);
        // Working tree untouched.
        assert_eq!(project.read(&p("src/auth.rs")).unwrap(), b"changed\n");
    }

    #[test]
    fn fetch_pull_and_push_against_a_local_remote() {
        let (_tmp, work, seed) = repo_with_remote();
        let git_service = GitService::new(&work);
        let status = git_service.status().unwrap();
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (0, 0));

        // Upstream moves on: fetch notices, pull fast-forwards.
        std::fs::write(seed.root().join("a.txt"), "two\n").unwrap();
        git(seed.root(), &["commit", "-q", "-am", "two"]);
        git(seed.root(), &["push", "-q"]);
        let fetched = git_service.fetch().unwrap();
        assert!(fetched.message.contains("1 behind"), "{fetched:?}");
        assert_eq!(git_service.status().unwrap().behind, 1);
        let pulled = git_service.pull().unwrap();
        assert!(pulled.message.contains("Pulled 1 commit"), "{pulled:?}");
        assert_eq!(work.read(&p("a.txt")).unwrap(), b"two\n");
        assert_eq!(git_service.status().unwrap().behind, 0);

        // Pull without fetching first: the stale `behind` is 0, but the
        // message counts what was actually pulled.
        for n in ["three", "four"] {
            std::fs::write(seed.root().join("a.txt"), format!("{n}\n")).unwrap();
            git(seed.root(), &["commit", "-q", "-am", n]);
        }
        git(seed.root(), &["push", "-q"]);
        assert_eq!(git_service.status().unwrap().behind, 0);
        let pulled = git_service.pull().unwrap();
        assert!(pulled.message.contains("Pulled 2 commits"), "{pulled:?}");
        assert_eq!(work.read(&p("a.txt")).unwrap(), b"four\n");
        let again = git_service.pull().unwrap();
        assert!(again.message.contains("already up to date"), "{again:?}");

        // A local commit: ahead 1, push sends it.
        work.write(&p("b.txt"), b"local\n").unwrap();
        git_service.stage_all().unwrap();
        git_service.commit("local").unwrap();
        assert_eq!(git_service.status().unwrap().ahead, 1);
        let pushed = git_service.push(false).unwrap();
        assert!(pushed.message.contains("Pushed 1 commit"), "{pushed:?}");
        assert_eq!(git_service.status().unwrap().ahead, 0);

        // A new branch has no upstream: push refuses until asked explicitly.
        git_service.create_branch("feature").unwrap();
        match git_service.push(false) {
            Err(GitError::UpstreamRequired(target)) => {
                assert_eq!(target.remote, "origin");
                assert_eq!(target.branch, "feature");
            }
            other => panic!("expected UpstreamRequired, got {other:?}"),
        }
        let published = git_service.push(true).unwrap();
        assert!(published.message.contains("origin/feature"));
        assert_eq!(
            git_service.status().unwrap().upstream.as_deref(),
            Some("origin/feature")
        );
        // Pull without an upstream is refused with a reason, not attempted.
        git_service.create_branch("lonely").unwrap();
        assert!(
            matches!(git_service.pull(), Err(GitError::Invalid(m)) if m.contains("no upstream"))
        );
    }

    #[test]
    fn pull_refuses_to_merge_diverged_history_and_push_never_forces() {
        let (_tmp, work, seed) = repo_with_remote();
        let git_service = GitService::new(&work);
        std::fs::write(seed.root().join("a.txt"), "upstream\n").unwrap();
        git(seed.root(), &["commit", "-q", "-am", "upstream"]);
        git(seed.root(), &["push", "-q"]);
        work.write(&p("c.txt"), b"mine\n").unwrap();
        git_service.stage_all().unwrap();
        git_service.commit("mine").unwrap();
        let head_before = git_service.log(None, 1).unwrap()[0].hash.clone();

        git_service.fetch().unwrap();
        let status = git_service.status().unwrap();
        assert_eq!((status.ahead, status.behind), (1, 1));
        let refused = git_service.pull().unwrap_err();
        assert!(
            matches!(&refused, GitError::Failed(m) if m.contains("diverged")),
            "{refused:?}"
        );
        // Nothing merged or rebased.
        assert_eq!(git_service.log(None, 1).unwrap()[0].hash, head_before);
        assert_eq!(work.read(&p("a.txt")).unwrap(), b"one\n");

        let rejected = git_service.push(false).unwrap_err();
        assert!(
            matches!(&rejected, GitError::Failed(m) if m.contains("never force")),
            "{rejected:?}"
        );
    }

    #[test]
    fn fetch_without_a_remote_explains_itself() {
        let (_tmp, project) = repo();
        let git_service = GitService::new(&project);
        assert!(
            matches!(git_service.fetch(), Err(GitError::Invalid(m)) if m.contains("no remote"))
        );
        assert!(matches!(git_service.pull(), Err(GitError::Invalid(_))));
        assert!(
            matches!(git_service.push(false), Err(GitError::Invalid(m)) if m.contains("no remote"))
        );
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
