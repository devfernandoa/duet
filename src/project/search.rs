//! Project search: listing a project's files (for fuzzy filename search),
//! fuzzy-matching a query against them, and searching file contents.
//!
//! Split into two layers so Milestone 11's command palette can reuse the
//! matching without re-implementing it, and so SSH/Docker projects don't need
//! a rewrite:
//!
//! - *Pure* functions — [`fuzzy_score`], [`fuzzy_filter`], [`parse_rg_json`],
//!   [`search_text`] — that only see strings and bytes.
//! - *Host-facing* functions — [`list_files`], [`search_content`] — that ask
//!   the project's own tools first (`rg`, then `git ls-files`) through
//!   [`ProjectCommands`] and fall back to walking the tree through
//!   [`ProjectFilesystem`]. Locally that means ripgrep when it's installed; on
//!   a remote host it means whatever that host has, with the same fallback.

use super::fs::{EntryKind, Project, looks_like_text};
use super::git::GitService;
use super::path::ProjectPath;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListOptions {
    pub include_hidden: bool,
    pub respect_gitignore: bool,
    /// Stop after this many files — a guard for enormous trees.
    pub limit: usize,
}

impl Default for ListOptions {
    fn default() -> Self {
        ListOptions {
            include_hidden: false,
            respect_gitignore: true,
            limit: 50_000,
        }
    }
}

/// Every file under `root` (recursively), as project paths, honoring
/// hidden-file and `.gitignore` options. Never includes `.git` itself.
pub fn list_files(
    project: &dyn Project,
    root: &ProjectPath,
    options: ListOptions,
) -> Result<Vec<ProjectPath>, String> {
    if let Some(files) = list_files_with_rg(project, root, options) {
        return Ok(files);
    }
    if options.respect_gitignore
        && let Some(files) = list_files_with_git(project, root, options)
    {
        return Ok(files);
    }
    walk_files(project, root, options)
}

fn list_files_with_rg(
    project: &dyn Project,
    root: &ProjectPath,
    options: ListOptions,
) -> Option<Vec<ProjectPath>> {
    let mut args = vec!["--files", "--no-messages", "--glob", "!.git"];
    if options.include_hidden {
        args.push("--hidden");
    }
    if !options.respect_gitignore {
        args.push("--no-ignore");
    }
    if !root.is_root() {
        args.push("--");
        args.push(root.as_str());
    }
    let output = project.run("rg", &args, None).ok()?;
    // 0 = files found, 1 = none found; 2 = error (but partial output may
    // still be useful, e.g. one unreadable directory).
    if !matches!(output.status, Some(0..=2)) {
        return None;
    }
    let mut files: Vec<ProjectPath> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| ProjectPath::parse(line).ok())
        .take(options.limit)
        .collect();
    files.sort();
    Some(files)
}

fn list_files_with_git(
    project: &dyn Project,
    root: &ProjectPath,
    options: ListOptions,
) -> Option<Vec<ProjectPath>> {
    if !GitService::new(project).is_repository() {
        return None;
    }
    let mut args = vec![
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ];
    if !root.is_root() {
        args.push("--");
        args.push(root.as_str());
    }
    let output = project.run("git", &args, None).ok()?;
    if !output.success() {
        return None;
    }
    let mut files: Vec<ProjectPath> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .filter_map(|chunk| ProjectPath::parse(&String::from_utf8_lossy(chunk)).ok())
        .filter(|path| options.include_hidden || !path.is_hidden())
        // `ls-files --cached` still lists a tracked file that was deleted
        // from the working tree; only report what's actually there.
        .filter(|path| project.exists(path))
        .take(options.limit)
        .collect();
    files.sort();
    files.dedup();
    Some(files)
}

/// The tool-less fallback: a plain recursive walk through
/// `ProjectFilesystem::list_dir`. `.gitignore` is approximated by asking git
/// (if present) which directories it ignores, one level at a time.
fn walk_files(
    project: &dyn Project,
    root: &ProjectPath,
    options: ListOptions,
) -> Result<Vec<ProjectPath>, String> {
    let git = GitService::new(project);
    let mut files = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        let entries = project.list_dir(&dir).map_err(|error| error.to_string())?;
        let candidates: Vec<_> = entries
            .into_iter()
            .filter(|entry| entry.name != ".git")
            .filter(|entry| options.include_hidden || !entry.is_hidden())
            .collect();
        let ignored = if options.respect_gitignore {
            git.ignored(
                &candidates
                    .iter()
                    .map(|e| e.path.clone())
                    .collect::<Vec<_>>(),
            )
        } else {
            Vec::new()
        };
        for entry in candidates {
            if ignored.contains(&entry.path) {
                continue;
            }
            match entry.kind {
                EntryKind::Directory => pending.push(entry.path),
                EntryKind::File => files.push(entry.path),
                EntryKind::Symlink | EntryKind::Other => {}
            }
            if files.len() >= options.limit {
                break;
            }
        }
    }
    files.sort();
    Ok(files)
}

/// One fuzzy filename hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameMatch {
    pub path: ProjectPath,
    pub score: i64,
    /// Character (not byte) indices into `path` that matched, for
    /// highlighting.
    pub positions: Vec<usize>,
}

fn is_boundary(previous: Option<char>, current: char) -> bool {
    match previous {
        None => true,
        Some(prev) => {
            matches!(prev, '/' | '_' | '-' | '.' | ' ')
                || (prev.is_lowercase() && current.is_uppercase())
        }
    }
}

/// Scores `candidate` against `query` as a case-insensitive subsequence
/// match, or `None` if `query`'s characters don't all appear in order.
/// Matches at word/path-segment boundaries and consecutive runs score
/// higher; matches inside the file name (after the last `/`) score higher
/// than ones in a directory; shorter candidates win ties. Whitespace in
/// `query` is ignored, so `auth rs` finds `src/auth.rs`.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<(i64, Vec<usize>)> {
    let needle: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }
    let haystack: Vec<char> = candidate.chars().collect();
    let name_start = candidate
        .rfind('/')
        .map(|byte| candidate[..=byte].chars().count())
        .unwrap_or(0);

    // Greedy forward match, preferring boundary positions when one is
    // reachable before the next plain occurrence would be used up.
    let mut positions = Vec::with_capacity(needle.len());
    let mut from = 0;
    for &wanted in &needle {
        let mut first = None;
        let mut boundary = None;
        for (index, &c) in haystack.iter().enumerate().skip(from) {
            if c.to_lowercase().eq(std::iter::once(wanted)) {
                if first.is_none() {
                    first = Some(index);
                }
                let previous = index.checked_sub(1).map(|i| haystack[i]);
                if is_boundary(previous, c) {
                    boundary = Some(index);
                    break;
                }
            }
        }
        // Only jump ahead to a boundary when it doesn't break up a run
        // that's already consecutive.
        let consecutive = positions
            .last()
            .is_some_and(|&last: &usize| first == Some(last + 1));
        let chosen = if consecutive {
            first
        } else {
            boundary.or(first)
        }?;
        positions.push(chosen);
        from = chosen + 1;
    }

    let mut score: i64 = 0;
    for (n, &index) in positions.iter().enumerate() {
        score += 10;
        let previous = index.checked_sub(1).map(|i| haystack[i]);
        if is_boundary(previous, haystack[index]) {
            score += 30;
        }
        if n > 0 && positions[n - 1] + 1 == index {
            score += 25;
        }
        if index >= name_start {
            score += 15;
        }
    }
    // Prefer tighter spans and shorter paths.
    let span = (positions.last().unwrap() - positions.first().unwrap() + 1) as i64;
    score -= span - needle.len() as i64;
    score -= haystack.len() as i64 / 4;
    Some((score, positions))
}

/// Ranks every path in `paths` against `query` and returns the best
/// `limit` hits, best first (ties broken by path, so results are stable).
pub fn fuzzy_filter(query: &str, paths: &[ProjectPath], limit: usize) -> Vec<NameMatch> {
    let mut matches: Vec<NameMatch> = paths
        .iter()
        .filter_map(|path| {
            let (score, positions) = fuzzy_score(query, path.as_str())?;
            Some(NameMatch {
                path: path.clone(),
                score,
                positions,
            })
        })
        .collect();
    matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    matches.truncate(limit);
    matches
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentSearchOptions {
    pub root: ProjectPath,
    pub include_hidden: bool,
    pub respect_gitignore: bool,
    /// Treat the pattern as a literal string rather than a regex. The
    /// tool-less fallback always searches literally.
    pub fixed_strings: bool,
    pub limit: usize,
}

impl Default for ContentSearchOptions {
    fn default() -> Self {
        ContentSearchOptions {
            root: ProjectPath::root(),
            include_hidden: false,
            respect_gitignore: true,
            fixed_strings: false,
            limit: 500,
        }
    }
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentMatch {
    pub path: ProjectPath,
    /// 1-based.
    pub line: u64,
    /// 1-based character column of the first match on the line.
    pub column: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSearchResult {
    pub matches: Vec<ContentMatch>,
    /// More matches existed than `limit` allowed.
    pub truncated: bool,
    /// Which engine answered: `"ripgrep"` or `"builtin"` — reported so a
    /// caller can tell a literal-only fallback search apart from a regex one.
    pub engine: String,
}

/// Searches file contents under `options.root` for `pattern`. Smart case:
/// an all-lowercase pattern matches case-insensitively.
pub fn search_content(
    project: &dyn Project,
    pattern: &str,
    options: &ContentSearchOptions,
) -> Result<ContentSearchResult, String> {
    if pattern.is_empty() {
        return Err("search needs a non-empty pattern".to_string());
    }
    let mut args = vec![
        "--json",
        "--smart-case",
        "--no-messages",
        "--sort=path",
        "--glob",
        "!.git",
    ];
    if options.include_hidden {
        args.push("--hidden");
    }
    if !options.respect_gitignore {
        args.push("--no-ignore");
    }
    if options.fixed_strings {
        args.push("--fixed-strings");
    }
    args.push("--regexp");
    args.push(pattern);
    if !options.root.is_root() {
        args.push("--");
        args.push(options.root.as_str());
    }
    match project.run("rg", &args, None) {
        Ok(output) if matches!(output.status, Some(0..=1)) => {
            let (matches, truncated) = parse_rg_json(&output.stdout, options.limit);
            Ok(ContentSearchResult {
                matches,
                truncated,
                engine: "ripgrep".to_string(),
            })
        }
        Ok(output) if output.status == Some(2) && output.stdout.is_empty() => {
            Err(format!("invalid search: {}", output.stderr_text()))
        }
        Ok(output) if output.status == Some(2) => {
            // Partial results (e.g. an unreadable file) are still results.
            let (matches, truncated) = parse_rg_json(&output.stdout, options.limit);
            Ok(ContentSearchResult {
                matches,
                truncated,
                engine: "ripgrep".to_string(),
            })
        }
        _ => builtin_search(project, pattern, options),
    }
}

/// Parses `rg --json` output into at most `limit` matches; the bool reports
/// whether more were available.
pub fn parse_rg_json(raw: &[u8], limit: usize) -> (Vec<ContentMatch>, bool) {
    let mut matches = Vec::new();
    for line in raw.split(|byte| *byte == b'\n') {
        let Ok(event) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if event["type"] != "match" {
            continue;
        }
        if matches.len() >= limit {
            return (matches, true);
        }
        let data = &event["data"];
        let Some(path) = data["path"]["text"]
            .as_str()
            .and_then(|raw| ProjectPath::parse(raw).ok())
        else {
            continue;
        };
        let text = data["lines"]["text"]
            .as_str()
            .unwrap_or_default()
            .trim_end_matches(['\n', '\r'])
            .to_string();
        let byte_start = data["submatches"][0]["start"].as_u64().unwrap_or(0) as usize;
        let column = text
            .get(..byte_start.min(text.len()))
            .map(|prefix| prefix.chars().count() as u64 + 1)
            .unwrap_or(1);
        matches.push(ContentMatch {
            path,
            line: data["line_number"].as_u64().unwrap_or(0),
            column,
            text,
        });
    }
    (matches, false)
}

/// Literal (smart-case) search of one file's text, for the builtin engine.
pub fn search_text(path: &ProjectPath, text: &str, pattern: &str) -> Vec<ContentMatch> {
    let insensitive = !pattern.chars().any(char::is_uppercase);
    let needle = if insensitive {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let haystack = if insensitive {
                line.to_lowercase()
            } else {
                line.to_string()
            };
            let byte = haystack.find(&needle)?;
            Some(ContentMatch {
                path: path.clone(),
                line: index as u64 + 1,
                column: haystack[..byte].chars().count() as u64 + 1,
                text: line.to_string(),
            })
        })
        .collect()
}

/// Files larger than this are skipped by the builtin engine.
const BUILTIN_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

fn builtin_search(
    project: &dyn Project,
    pattern: &str,
    options: &ContentSearchOptions,
) -> Result<ContentSearchResult, String> {
    let files = list_files(
        project,
        &options.root,
        ListOptions {
            include_hidden: options.include_hidden,
            respect_gitignore: options.respect_gitignore,
            ..ListOptions::default()
        },
    )?;
    let mut matches = Vec::new();
    for path in files {
        if project
            .metadata(&path)
            .is_ok_and(|m| m.size > BUILTIN_MAX_FILE_BYTES)
        {
            continue;
        }
        let Ok(bytes) = project.read(&path) else {
            continue;
        };
        if !looks_like_text(&bytes[..bytes.len().min(8192)]) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for hit in search_text(&path, &text, pattern) {
            if matches.len() >= options.limit {
                return Ok(ContentSearchResult {
                    matches,
                    truncated: true,
                    engine: "builtin".to_string(),
                });
            }
            matches.push(hit);
        }
    }
    Ok(ContentSearchResult {
        matches,
        truncated: false,
        engine: "builtin".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::fs::{
        CommandOutput, FsResult, LocalProject, ProjectCommands, ProjectFilesystem,
    };
    use crate::project::fs::{DirEntry, FileMetadata};
    use crate::project::git::tests::{p, repo};

    /// Wraps a real local project but pretends no external tool is
    /// installed, to exercise the builtin fallbacks deterministically.
    struct NoTools(LocalProject);

    impl ProjectFilesystem for NoTools {
        fn root_label(&self) -> String {
            self.0.root_label()
        }
        fn metadata(&self, path: &ProjectPath) -> FsResult<FileMetadata> {
            self.0.metadata(path)
        }
        fn list_dir(&self, path: &ProjectPath) -> FsResult<Vec<DirEntry>> {
            self.0.list_dir(path)
        }
        fn read(&self, path: &ProjectPath) -> FsResult<Vec<u8>> {
            self.0.read(path)
        }
        fn write(&self, path: &ProjectPath, contents: &[u8]) -> FsResult<()> {
            self.0.write(path, contents)
        }
    }

    impl ProjectCommands for NoTools {
        fn run(&self, _: &str, _: &[&str], _: Option<&[u8]>) -> std::io::Result<CommandOutput> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no tools",
            ))
        }
    }

    fn rg_available() -> bool {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn populated() -> (tempfile::TempDir, LocalProject) {
        let (tmp, project) = repo();
        project
            .write(&p("src/auth/session.rs"), b"pub fn TokenStore() {}\n")
            .unwrap();
        project
            .write(&p("docs/auth.md"), b"# Auth\nlogin flow\n")
            .unwrap();
        project
            .write(&p("target/debug/auth.rs"), b"fn login() {}\n")
            .unwrap();
        project
            .write(&p(".github/ci.yml"), b"login: true\n")
            .unwrap();
        (tmp, project)
    }

    #[test]
    fn fuzzy_score_prefers_boundaries_names_and_tight_matches() {
        assert!(fuzzy_score("xyz", "src/auth.rs").is_none());
        let (_, positions) = fuzzy_score("auth", "src/auth.rs").unwrap();
        assert_eq!(positions, vec![4, 5, 6, 7]);
        let file_name = fuzzy_score("auth", "src/auth.rs").unwrap().0;
        let scattered = fuzzy_score("auth", "a/u/t/h/x.rs").unwrap().0;
        assert!(file_name > scattered);
        assert!(
            fuzzy_score("ar", "src/auth.rs").unwrap().0
                > fuzzy_score("ar", "src/xxaxxr.rs").unwrap().0
        );
        assert!(
            fuzzy_score("SAR", "src/auth.rs").is_some(),
            "case-insensitive"
        );
        assert!(
            fuzzy_score("auth rs", "src/auth.rs").is_some(),
            "spaces ignored"
        );
        assert_eq!(fuzzy_score("", "anything"), Some((0, vec![])));
    }

    #[test]
    fn fuzzy_filter_ranks_and_limits() {
        let paths = vec![
            p("docs/authentication.md"),
            p("src/auth.rs"),
            p("src/main.rs"),
        ];
        let hits = fuzzy_filter("auth.rs", &paths, 10);
        assert_eq!(hits[0].path, p("src/auth.rs"));
        assert!(hits.iter().all(|hit| hit.path != p("src/main.rs")));
        assert_eq!(fuzzy_filter("s", &paths, 1).len(), 1);
    }

    #[test]
    fn list_files_respects_gitignore_and_hidden_with_and_without_tools() {
        let (_tmp, project) = populated();
        let expected = vec![
            p("docs/auth.md"),
            p("src/auth.rs"),
            p("src/auth/session.rs"),
        ];
        let options = ListOptions::default();
        let fallback = NoTools(project.clone());
        assert_eq!(
            list_files(&fallback, &ProjectPath::root(), options).unwrap(),
            {
                // The tool-less walk can't consult git for ignores either, so it
                // only hides hidden files.
                let mut all = expected.clone();
                all.push(p("target/debug/auth.rs"));
                all.sort();
                all
            }
        );
        if rg_available() {
            assert_eq!(
                list_files(&project, &ProjectPath::root(), options).unwrap(),
                expected
            );
            let with_hidden = list_files(
                &project,
                &ProjectPath::root(),
                ListOptions {
                    include_hidden: true,
                    ..options
                },
            )
            .unwrap();
            assert!(with_hidden.contains(&p(".github/ci.yml")));
            assert!(with_hidden.contains(&p(".gitignore")));
            assert!(
                !with_hidden
                    .iter()
                    .any(|path| path.as_str().starts_with(".git/"))
            );
            let scoped = list_files(&project, &p("src"), options).unwrap();
            assert_eq!(scoped, vec![p("src/auth.rs"), p("src/auth/session.rs")]);
        }
        // The git-based path, used when rg is missing but git isn't.
        let via_git = list_files_with_git(&project, &ProjectPath::root(), options).unwrap();
        assert_eq!(via_git, expected);
    }

    #[test]
    fn content_search_finds_lines_with_both_engines() {
        let (_tmp, project) = populated();
        let options = ContentSearchOptions::default();
        let builtin = search_content(&NoTools(project.clone()), "login", &options).unwrap();
        assert_eq!(builtin.engine, "builtin");
        assert!(
            builtin
                .matches
                .iter()
                .any(|m| m.path == p("src/auth.rs") && m.line == 1)
        );
        assert!(
            builtin
                .matches
                .iter()
                .any(|m| m.path == p("docs/auth.md") && m.line == 2)
        );

        if rg_available() {
            let result = search_content(&project, "login", &options).unwrap();
            assert_eq!(result.engine, "ripgrep");
            let found: Vec<(String, u64, u64)> = result
                .matches
                .iter()
                .map(|m| (m.path.to_string(), m.line, m.column))
                .collect();
            // Gitignored target/ and hidden .github/ are excluded.
            assert_eq!(
                found,
                vec![
                    ("docs/auth.md".to_string(), 2, 1),
                    ("src/auth.rs".to_string(), 1, 4)
                ]
            );
            let limited = search_content(
                &project,
                "login",
                &ContentSearchOptions {
                    limit: 1,
                    ..options.clone()
                },
            )
            .unwrap();
            assert_eq!(limited.matches.len(), 1);
            assert!(limited.truncated);
            // Smart case: an uppercase pattern is case-sensitive.
            assert!(
                search_content(&project, "LOGIN", &options)
                    .unwrap()
                    .matches
                    .is_empty()
            );
            assert!(search_content(&project, "(", &options).is_err());
        }
        assert!(search_content(&project, "", &options).is_err());
    }

    #[test]
    fn parse_rg_json_reads_match_events_only() {
        let raw = br#"{"type":"begin","data":{"path":{"text":"a.rs"}}}
{"type":"match","data":{"path":{"text":"a.rs"},"lines":{"text":"  let x = foo();\n"},"line_number":7,"absolute_offset":0,"submatches":[{"match":{"text":"foo"},"start":10,"end":13}]}}
{"type":"end","data":{}}
"#;
        let (matches, truncated) = parse_rg_json(raw, 10);
        assert!(!truncated);
        assert_eq!(
            matches,
            vec![ContentMatch {
                path: p("a.rs"),
                line: 7,
                column: 11,
                text: "  let x = foo();".to_string()
            }]
        );
        assert!(parse_rg_json(raw, 0).1);
    }

    #[test]
    fn search_text_is_smart_case_and_literal() {
        let hits = search_text(&p("a.txt"), "Foo bar\nfoo.*\nnothing", "foo.*");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
        assert_eq!(search_text(&p("a.txt"), "Foo\nfoo", "foo").len(), 2);
        assert_eq!(search_text(&p("a.txt"), "Foo\nfoo", "Foo").len(), 1);
    }
}
