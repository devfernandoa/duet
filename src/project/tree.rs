//! The FileTree node's view model: which rows a tree shows for a given root,
//! set of expanded directories, hidden/`.gitignore` options and Git status —
//! computed purely from a [`Project`], so the expand/collapse/filter/marker
//! behavior is unit tested here and the GTK widget (`node_files.rs`) only
//! draws the rows it's handed.

use super::fs::{EntryKind, Project};
use super::git::{GitService, GitStatus};
use super::path::ProjectPath;
use std::collections::BTreeSet;

/// Rows beyond this are not built — a guard against expanding something like
/// `node_modules` into a widget with a hundred thousand children.
pub const MAX_TREE_ROWS: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeOptions {
    pub show_hidden: bool,
    pub respect_gitignore: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub path: ProjectPath,
    pub name: String,
    /// 0 for a direct child of the tree's root.
    pub depth: usize,
    pub kind: EntryKind,
    pub expanded: bool,
    /// The Git marker to show: a file's own status letter (`M`, `A`, `?`,
    /// ...), or `•` on a directory with changes somewhere beneath it.
    pub marker: Option<char>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeView {
    pub rows: Vec<TreeRow>,
    /// `MAX_TREE_ROWS` was hit; some rows are missing.
    pub truncated: bool,
}

/// Builds the visible rows under `root`: its entries, and recursively the
/// entries of every directory in `expanded`. `.git` is never shown.
pub fn build_tree(
    project: &dyn Project,
    root: &ProjectPath,
    expanded: &BTreeSet<ProjectPath>,
    options: TreeOptions,
    status: Option<&GitStatus>,
) -> Result<TreeView, String> {
    let git = GitService::new(project);
    let mut view = TreeView::default();
    append_children(project, &git, root, 0, expanded, options, status, &mut view)?;
    Ok(view)
}

#[allow(clippy::too_many_arguments)]
fn append_children(
    project: &dyn Project,
    git: &GitService,
    dir: &ProjectPath,
    depth: usize,
    expanded: &BTreeSet<ProjectPath>,
    options: TreeOptions,
    status: Option<&GitStatus>,
    view: &mut TreeView,
) -> Result<(), String> {
    let entries = project.list_dir(dir).map_err(|error| error.to_string())?;
    let visible: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.name != ".git")
        .filter(|entry| options.show_hidden || !entry.is_hidden())
        .collect();
    let ignored = if options.respect_gitignore {
        git.ignored(&visible.iter().map(|e| e.path.clone()).collect::<Vec<_>>())
    } else {
        Vec::new()
    };
    for entry in visible {
        if ignored.contains(&entry.path) {
            continue;
        }
        if view.rows.len() >= MAX_TREE_ROWS {
            view.truncated = true;
            return Ok(());
        }
        let is_dir = entry.kind == EntryKind::Directory;
        let is_expanded = is_dir && expanded.contains(&entry.path);
        let marker = status.and_then(|status| {
            if is_dir {
                status.has_changes_under(&entry.path).then_some('•')
            } else {
                status.entry(&entry.path).map(|e| e.marker())
            }
        });
        let path = entry.path.clone();
        view.rows.push(TreeRow {
            path: entry.path,
            name: entry.name,
            depth,
            kind: entry.kind,
            expanded: is_expanded,
            marker,
        });
        if is_expanded {
            // An expanded directory that became unreadable shouldn't take
            // the whole tree down with it.
            let _ = append_children(
                project,
                git,
                &path,
                depth + 1,
                expanded,
                options,
                status,
                view,
            );
        }
    }
    Ok(())
}

/// Expands every ancestor of `path` (below the tree root) so it becomes
/// visible — used when a search hit or "reveal" selects a deep file.
pub fn reveal(expanded: &mut BTreeSet<ProjectPath>, root: &ProjectPath, path: &ProjectPath) {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == *root || !dir.starts_with(root) {
            break;
        }
        expanded.insert(dir.clone());
        current = dir.parent();
    }
}

/// Back/forward history over a FileTree's root directory. Runtime-only:
/// the current root itself is persisted on the node, the history isn't.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavigationHistory {
    back: Vec<ProjectPath>,
    forward: Vec<ProjectPath>,
}

impl NavigationHistory {
    /// Records leaving `current` for somewhere new; clears forward history.
    pub fn navigate(&mut self, current: ProjectPath) {
        self.back.push(current);
        self.forward.clear();
    }

    pub fn back(&mut self, current: ProjectPath) -> Option<ProjectPath> {
        let previous = self.back.pop()?;
        self.forward.push(current);
        Some(previous)
    }

    pub fn forward(&mut self, current: ProjectPath) -> Option<ProjectPath> {
        let next = self.forward.pop()?;
        self.back.push(current);
        Some(next)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::fs::ProjectFilesystem;
    use crate::project::git::tests::{p, repo};

    fn names(view: &TreeView) -> Vec<String> {
        view.rows
            .iter()
            .map(|row| {
                format!(
                    "{}{}{}",
                    "  ".repeat(row.depth),
                    row.name,
                    row.marker.map(|m| format!(" [{m}]")).unwrap_or_default()
                )
            })
            .collect()
    }

    #[test]
    fn tree_expands_filters_and_marks() {
        let (_tmp, project) = repo();
        project.write(&p("src/auth.rs"), b"changed\n").unwrap();
        project.write(&p("src/new.rs"), b"new\n").unwrap();
        project.write(&p("target/out.bin"), b"x").unwrap();
        project.write(&p("README.md"), b"# hi\n").unwrap();
        let status = GitService::new(&project).status().unwrap();
        let options = TreeOptions {
            show_hidden: false,
            respect_gitignore: true,
        };

        let collapsed = build_tree(
            &project,
            &ProjectPath::root(),
            &BTreeSet::new(),
            options,
            Some(&status),
        )
        .unwrap();
        assert_eq!(names(&collapsed), vec!["src [•]", "README.md [?]"]);

        let expanded: BTreeSet<_> = [p("src")].into_iter().collect();
        let open = build_tree(
            &project,
            &ProjectPath::root(),
            &expanded,
            options,
            Some(&status),
        )
        .unwrap();
        assert_eq!(
            names(&open),
            vec!["src [•]", "  auth.rs [M]", "  new.rs [?]", "README.md [?]"]
        );
        assert!(open.rows[0].expanded);

        let everything = build_tree(
            &project,
            &ProjectPath::root(),
            &BTreeSet::new(),
            TreeOptions {
                show_hidden: true,
                respect_gitignore: false,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            names(&everything),
            vec!["src", "target", ".gitignore", "README.md"]
        );

        let scoped = build_tree(&project, &p("src"), &BTreeSet::new(), options, None).unwrap();
        assert_eq!(names(&scoped), vec!["auth.rs", "new.rs"]);
    }

    #[test]
    fn reveal_expands_only_ancestors_below_the_root() {
        let mut expanded = BTreeSet::new();
        reveal(&mut expanded, &ProjectPath::root(), &p("a/b/c.rs"));
        assert_eq!(expanded, [p("a"), p("a/b")].into_iter().collect());
        let mut scoped = BTreeSet::new();
        reveal(&mut scoped, &p("a"), &p("a/b/c.rs"));
        assert_eq!(scoped, [p("a/b")].into_iter().collect());
    }

    #[test]
    fn navigation_history_goes_back_and_forward() {
        let mut history = NavigationHistory::default();
        assert!(!history.can_go_back());
        history.navigate(p(""));
        history.navigate(p("src"));
        // Now at src/auth.
        assert_eq!(history.back(p("src/auth")), Some(p("src")));
        assert_eq!(history.back(p("src")), Some(p("")));
        assert_eq!(history.back(p("")), None);
        assert_eq!(history.forward(p("")), Some(p("src")));
        assert!(history.can_go_forward());
        history.navigate(p("docs"));
        assert!(!history.can_go_forward());
    }
}
