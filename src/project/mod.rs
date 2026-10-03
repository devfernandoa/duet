//! Milestone 6's project filesystem layer: everything Duet knows about the
//! files of a workspace's project root, GTK-free and host-agnostic.
//!
//! - [`path`]: normalized, root-relative [`ProjectPath`]s (traversal is a
//!   parse error) and `@file:` reference formatting.
//! - [`fs`]: the [`ProjectFilesystem`]/[`ProjectCommands`] boundary and its
//!   one implementation today, [`LocalProject`].
//! - [`search`]: fuzzy filename and content search.
//! - [`git`]: the Git service.
//! - [`sync`]: content revisions and the conflict-safe sync policy used by
//!   file-backed Notes and the editor.
//! - [`tree`]: the FileTree node's view model.
//! - [`service`]: inspect/read/write as `duetctl file` and the GUI use them.
//!
//! Dependency direction (CLAUDE.md): this module depends on nothing above
//! it — no GTK, no `App`, no orchestration. `orchestration::resource`
//! (`@file:` resolution), `control.rs` (`duetctl file`/`git`) and the GTK
//! layer all call down into it.

pub mod fs;
pub mod git;
pub mod path;
pub mod search;
pub mod service;
pub mod sync;
pub mod tree;

pub use fs::{FileClass, LocalProject, Project, ProjectCommands, ProjectFilesystem};
pub use git::GitService;
pub use path::{LineRange, ProjectPath};
pub use sync::FileRevision;
