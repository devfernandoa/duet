//! `App`'s project-file services (Milestone 6) and the GTK wiring of the
//! FileTree, Editor and file-backed Note nodes.
//!
//! The `pub fn`s on `App` here are the `FileService`/`GitService`
//! application-service entry points `control.rs` (`duetctl file`/`git`) and
//! the GTK callbacks below *both* call — the same split `app.rs`'s module doc
//! comment describes for messaging and notes. All real file/Git/search/sync
//! behavior lives in the GTK-free `project` module; this file only resolves
//! the active workspace's project root, keeps per-node runtime state
//! (`FileSyncState`, `TreeState` — never persisted), and moves data between
//! `project::*` and the widgets in `node_files.rs`/`node.rs`.
//!
//! Every handler follows the same borrow discipline as the rest of `app.rs`:
//! read what it needs from `App` in a short borrow, drop it, *then* touch
//! GTK — a widget update can synchronously emit a signal whose handler
//! borrows `App` again.

use super::{App, CanvasCommand, NodeWidget, materialize_node, next_z_order};
use crate::canvas;
use crate::model::{
    EditorPayload, FileTreePayload, FloorRef, NodeKind, NodeRecord, NoteFileBacking, NotePayload,
    NoteViewMode,
};
use crate::node::NoteNode;
use crate::node_files::{EditorDisplay, EditorNode, FILE_DRAG_PREFIX, FileTreeItem, FileTreeNode};
use crate::orchestration::resource::{ResourceKind, ResourceRef, is_path_like};
use crate::project::fs::{DirEntry, EntryKind, FileClass, ProjectFilesystem};
use crate::project::git::{DiffScope, GitLogEntry, GitService, GitStatus};
use crate::project::path::{
    LineRange, ProjectPath, diff_reference, file_reference, split_line_suffix,
};
use crate::project::search::{
    ContentSearchOptions, ContentSearchResult, ListOptions, NameMatch, fuzzy_filter, list_files,
    search_content,
};
use crate::project::service::{FileContent, FileInfo};
use crate::project::sync::{
    ConflictKind, DiskState, FileRevision, SyncAction, disk_state, reconcile,
};
use crate::project::tree::{NavigationHistory, TreeOptions, build_tree, reveal};
use crate::project::{LocalProject, service};
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use uuid::Uuid;

/// Which prompt a file-backed node's banner is currently showing — decides
/// what its two buttons do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncBanner {
    /// A file-backed note and its file both changed.
    NoteConflict,
    /// A file-backed note's file was deleted.
    NoteDeleted,
    /// An editor's file changed on disk while it had unsaved edits.
    EditorChangedWhileDirty,
    /// An editor save found the file changed since it was loaded.
    EditorSaveConflict,
    /// An editor's file was deleted on disk.
    EditorDeleted,
}

/// A cheap change signature (`modified`, `size`) — `None` when the file is
/// missing.
type Signature = Option<(Option<u64>, u64)>;

/// Runtime sync state of one file-backed Note or Editor. Not persisted: a
/// note's durable base revision lives on its `NoteFileBacking`; an editor
/// re-reads its file on open.
#[derive(Debug, Default)]
pub struct FileSyncState {
    /// Editors only: the revision the buffer was loaded from or last saved
    /// as. (A note's base is `NoteFileBacking::revision`.)
    pub base: Option<FileRevision>,
    /// The signature at the last check — when unchanged (and nothing local
    /// changed) a sync tick skips reading/hashing the file entirely.
    pub last_seen: Option<Signature>,
    pub banner: Option<SyncBanner>,
}

/// Runtime state of one FileTree node: navigation history, the active
/// search query, and the cached file list fuzzy search runs over.
#[derive(Debug, Default)]
pub struct TreeState {
    pub history: NavigationHistory,
    pub query: String,
    pub files: Option<Vec<ProjectPath>>,
}

/// How often (in sync ticks, see `main.rs`) idle FileTrees re-check Git
/// status and directory contents.
const TREE_REFRESH_TICKS: u32 = 3;

/// Maximum fuzzy-search hits / content matches shown in a FileTree.
const TREE_SEARCH_LIMIT: usize = 200;

fn signature(project: &LocalProject, path: &ProjectPath) -> Signature {
    project
        .metadata(path)
        .ok()
        .map(|metadata| (metadata.modified, metadata.size))
}

fn parse_root(raw: &str) -> ProjectPath {
    ProjectPath::parse(raw).unwrap_or_default()
}

/// The window a widget lives in, for parenting dialogs.
fn window_of(widget: &impl IsA<gtk4::Widget>) -> Option<gtk4::Window> {
    widget.root().and_downcast::<gtk4::Window>()
}

fn toast(toast_overlay: &adw::ToastOverlay, message: &str) {
    toast_overlay.add_toast(adw::Toast::new(message));
}

impl App {
    /// The active workspace's project, rooted at its `root_dir`.
    pub fn project(&self) -> LocalProject {
        LocalProject::new(self.workspace_root.clone())
    }

    /// Parses a file argument as `duetctl` and the GUI accept it: a
    /// project-relative path (`src/a.rs`, optionally `#L10-20`), an
    /// `@file:` reference, a path-like `@src/a.rs`, or an absolute path
    /// inside the project root.
    pub fn parse_file_argument(
        &self,
        raw: &str,
    ) -> Result<(ProjectPath, Option<LineRange>), String> {
        let raw = raw.trim();
        if raw.starts_with('@') {
            let reference = ResourceRef::parse(raw)?;
            return match reference.kind {
                Some(ResourceKind::File) => Ok((
                    ProjectPath::parse(&reference.name).map_err(|e| e.to_string())?,
                    reference.lines,
                )),
                None if is_path_like(&reference.name) => Ok((
                    ProjectPath::parse(&reference.name).map_err(|e| e.to_string())?,
                    reference.lines,
                )),
                Some(ResourceKind::Diff) => Err(format!(
                    "{raw} is a diff reference; use `duetctl git diff {}`",
                    reference.name
                )),
                _ => Err(format!("{raw} is not a file reference")),
            };
        }
        let (path, lines) = split_line_suffix(raw)?;
        if path.starts_with('/') {
            let absolute = std::path::Path::new(path);
            let candidates = [
                Some(self.workspace_root.clone()),
                self.workspace_root.canonicalize().ok(),
            ];
            for root in candidates.into_iter().flatten() {
                if let Ok(relative) = ProjectPath::from_absolute(&root, absolute) {
                    return Ok((relative, lines));
                }
            }
            return Err(format!(
                "{path} is outside the project root {}",
                self.workspace_root.display()
            ));
        }
        Ok((ProjectPath::parse(path).map_err(|e| e.to_string())?, lines))
    }

    /// `duetctl file inspect`'s service.
    pub fn inspect_file(&self, raw: &str) -> Result<FileInfo, String> {
        let (path, _) = self.parse_file_argument(raw)?;
        service::inspect(&self.project(), &path)
    }

    /// `duetctl file read`'s service. An explicit `lines` overrides a
    /// selection carried by the reference itself.
    pub fn read_file(&self, raw: &str, lines: Option<LineRange>) -> Result<FileContent, String> {
        let (path, reference_lines) = self.parse_file_argument(raw)?;
        service::read_text(&self.project(), &path, lines.or(reference_lines))
    }

    /// `duetctl file list`'s service: one directory's entries.
    pub fn list_directory(
        &self,
        raw: Option<&str>,
        include_hidden: bool,
    ) -> Result<Vec<DirEntry>, String> {
        let path = match raw {
            Some(raw) => self.parse_file_argument(raw)?.0,
            None => ProjectPath::root(),
        };
        let entries = self
            .project()
            .list_dir(&path)
            .map_err(|error| error.to_string())?;
        Ok(entries
            .into_iter()
            .filter(|entry| entry.name != ".git")
            .filter(|entry| include_hidden || !entry.is_hidden())
            .collect())
    }

    /// `duetctl file search <query>`'s service: fuzzy filename search.
    pub fn find_files(
        &self,
        query: &str,
        limit: usize,
        include_hidden: bool,
    ) -> Result<Vec<NameMatch>, String> {
        let project = self.project();
        let files = list_files(
            &project,
            &ProjectPath::root(),
            ListOptions {
                include_hidden,
                ..ListOptions::default()
            },
        )?;
        Ok(fuzzy_filter(query, &files, limit))
    }

    /// `duetctl file search --content <pattern>`'s service.
    pub fn search_files(
        &self,
        pattern: &str,
        fixed_strings: bool,
        limit: usize,
        include_hidden: bool,
    ) -> Result<ContentSearchResult, String> {
        search_content(
            &self.project(),
            pattern,
            &ContentSearchOptions {
                include_hidden,
                fixed_strings,
                limit,
                ..ContentSearchOptions::default()
            },
        )
    }

    /// `duetctl file write`'s service: an optimistic-concurrency write.
    /// `expected` must be the revision the new content was based on (from
    /// `file read`/`file inspect`) unless `create` is set, in which case the
    /// file must not exist yet — so an agent can never blindly clobber a
    /// file someone else just changed. Open editors and file-backed notes
    /// pick the change up on their next sync tick.
    pub fn write_file(
        app: &Rc<RefCell<App>>,
        raw: &str,
        expected: Option<FileRevision>,
        create: bool,
        content: &str,
    ) -> Result<FileRevision, String> {
        let (path, _) = app.borrow().parse_file_argument(raw)?;
        if expected.is_none() && !create {
            return Err(
                "pass --revision <rev> (from `duetctl file read`/`inspect`) to overwrite, or --create for a new file"
                    .to_string(),
            );
        }
        let project = app.borrow().project();
        let revision = service::write_text(&project, &path, expected.as_ref(), content)
            .map_err(|error| error.to_string())?;
        App::refresh_file_trees(app);
        Ok(revision)
    }

    pub fn git_status(&self) -> Result<GitStatus, String> {
        GitService::new(&self.project())
            .status()
            .map_err(|error| error.to_string())
    }

    pub fn git_diff(&self, raw: Option<&str>, scope: DiffScope) -> Result<String, String> {
        let path = raw.map(|raw| self.parse_diff_argument(raw)).transpose()?;
        GitService::new(&self.project())
            .diff(path.as_ref(), scope)
            .map_err(|error| error.to_string())
    }

    /// Like `parse_file_argument`, but also accepts `@diff:path` and `.`.
    fn parse_diff_argument(&self, raw: &str) -> Result<ProjectPath, String> {
        if let Some(body) = raw.trim().strip_prefix("@diff:") {
            return ProjectPath::parse(body).map_err(|e| e.to_string());
        }
        Ok(self.parse_file_argument(raw)?.0)
    }

    pub fn git_log(&self, raw: Option<&str>, limit: usize) -> Result<Vec<GitLogEntry>, String> {
        let path = raw.map(|raw| self.parse_file_argument(raw)).transpose()?;
        GitService::new(&self.project())
            .log(path.as_ref().map(|(path, _)| path), limit)
            .map_err(|error| error.to_string())
    }

    fn parse_paths(&self, raws: &[String]) -> Result<Vec<ProjectPath>, String> {
        raws.iter()
            .map(|raw| self.parse_diff_argument(raw))
            .collect()
    }

    pub fn git_stage(app: &Rc<RefCell<App>>, raws: &[String]) -> Result<(), String> {
        let (project, paths) = {
            let app_ref = app.borrow();
            (app_ref.project(), app_ref.parse_paths(raws)?)
        };
        GitService::new(&project)
            .stage(&paths)
            .map_err(|error| error.to_string())?;
        App::after_git_change(app);
        Ok(())
    }

    pub fn git_unstage(app: &Rc<RefCell<App>>, raws: &[String]) -> Result<(), String> {
        let (project, paths) = {
            let app_ref = app.borrow();
            (app_ref.project(), app_ref.parse_paths(raws)?)
        };
        GitService::new(&project)
            .unstage(&paths)
            .map_err(|error| error.to_string())?;
        App::after_git_change(app);
        Ok(())
    }

    /// Discards working-tree changes; refuses unless `confirmed` (the GUI
    /// asks first; the CLI needs `--confirm`).
    pub fn git_discard(
        app: &Rc<RefCell<App>>,
        raws: &[String],
        confirmed: bool,
    ) -> Result<(), String> {
        let (project, paths) = {
            let app_ref = app.borrow();
            (app_ref.project(), app_ref.parse_paths(raws)?)
        };
        GitService::new(&project)
            .discard(&paths, confirmed)
            .map_err(|error| error.to_string())?;
        App::after_git_change(app);
        Ok(())
    }

    pub fn git_commit(app: &Rc<RefCell<App>>, message: &str) -> Result<String, String> {
        let project = app.borrow().project();
        let hash = GitService::new(&project)
            .commit(message)
            .map_err(|error| error.to_string())?;
        App::after_git_change(app);
        Ok(hash)
    }

    /// Git state changed: refresh every tree's markers and every open diff
    /// view.
    fn after_git_change(app: &Rc<RefCell<App>>) {
        App::refresh_file_trees(app);
        let diff_editors: Vec<Uuid> = app
            .borrow()
            .nodes
            .iter()
            .filter(
                |(_, entry)| matches!(&entry.record.kind, NodeKind::Editor(e) if e.diff.is_some()),
            )
            .map(|(id, _)| *id)
            .collect();
        for id in diff_editors {
            App::load_editor(app, id);
        }
    }

    /// Materializes `record` as a new undoable node and persists.
    fn add_file_node(app: &Rc<RefCell<App>>, record: NodeRecord) -> Result<Uuid, String> {
        let id = record.id;
        materialize_node(app, record.clone(), &adw::ToastOverlay::new())?;
        App::push_undo(
            app,
            CanvasCommand::AddNodes {
                nodes: vec![record],
                edges: Vec::new(),
            },
        );
        let _ = app.borrow().persist();
        Ok(id)
    }

    fn new_record(
        app: &Rc<RefCell<App>>,
        position: (f64, f64),
        size: (f64, f64),
        kind: NodeKind,
    ) -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position,
            size,
            z_order: next_z_order(&app.borrow()),
            collapsed: false,
            locked: false,
            kind,
        }
    }

    /// Adds a FileTree of the project root. Several can coexist, each with
    /// its own root, expansion and filter state.
    pub fn create_file_tree(app: &Rc<RefCell<App>>, position: (f64, f64)) -> Result<Uuid, String> {
        let record = App::new_record(
            app,
            position,
            (320.0, 460.0),
            NodeKind::FileTree(FileTreePayload::default()),
        );
        App::add_file_node(app, record)
    }

    /// Opens a project file on the canvas the way its content calls for: a
    /// Markdown file becomes a file-backed Note, other text opens in the
    /// editor, an image opens as an image; anything else is refused with a
    /// clear error rather than shown as garbage.
    pub fn open_project_file(
        app: &Rc<RefCell<App>>,
        path: &ProjectPath,
        position: (f64, f64),
        line: Option<u32>,
    ) -> Result<Uuid, String> {
        let project = app.borrow().project();
        match service::classify_file(&project, path)? {
            FileClass::Markdown if line.is_none() => App::open_file_as_note(app, path, position),
            FileClass::Markdown | FileClass::Text | FileClass::Image => {
                App::open_editor(app, path, None, position, line)
            }
            FileClass::Binary => Err(format!(
                "{path} is a binary file; Duet can't display it (reference it as {} instead)",
                file_reference(path, None)
            )),
        }
    }

    /// Opens `path` in an editor node — or, with `diff`, its Git diff in a
    /// read-only one.
    pub fn open_editor(
        app: &Rc<RefCell<App>>,
        path: &ProjectPath,
        diff: Option<DiffScope>,
        position: (f64, f64),
        line: Option<u32>,
    ) -> Result<Uuid, String> {
        let record = App::new_record(
            app,
            position,
            (640.0, 480.0),
            NodeKind::Editor(EditorPayload {
                path: path.display().to_string(),
                diff,
            }),
        );
        let id = App::add_file_node(app, record)?;
        if let Some(line) = line {
            let editor = app
                .borrow()
                .nodes
                .get(&id)
                .and_then(|entry| match &entry.widget {
                    NodeWidget::Editor(editor) => Some(editor.clone()),
                    _ => None,
                });
            if let Some(editor) = editor {
                glib::idle_add_local_once(move || editor.go_to_line(line));
            }
        }
        Ok(id)
    }

    /// Opens a Markdown file as a file-backed Note.
    pub fn open_file_as_note(
        app: &Rc<RefCell<App>>,
        path: &ProjectPath,
        position: (f64, f64),
    ) -> Result<Uuid, String> {
        let project = app.borrow().project();
        let content = service::read_text(&project, path, None)?;
        // `read_text` decodes lossily; a note must hold the exact bytes, or
        // its first sync would write the replacement characters back.
        if FileRevision::of(content.content.as_bytes()) != content.revision {
            return Err(format!(
                "{path} is not valid UTF-8 text; open it in the editor instead"
            ));
        }
        let record = App::new_record(
            app,
            position,
            (420.0, 360.0),
            NodeKind::Note(NotePayload {
                markdown: content.content,
                color: "yellow".to_string(),
                view_mode: NoteViewMode::Preview,
                file: Some(NoteFileBacking {
                    path: path.display().to_string(),
                    revision: Some(content.revision),
                }),
            }),
        );
        App::add_file_node(app, record)
    }

    /// `duetctl notes attach`'s service (and the note card's link-to-file
    /// popover): associates a note with a project file. If the file doesn't
    /// exist it's created from the note; if it exists with different
    /// content, the first sync reports a conflict and the user chooses — the
    /// note never silently overwrites it. Gated by `WriteNote` like every
    /// other note write.
    pub fn attach_note_file(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        raw: &str,
    ) -> Result<String, String> {
        let path = {
            let app_ref = app.borrow();
            app_ref
                .nodes
                .get(&id)
                .and_then(|entry| entry.record.as_note())
                .ok_or_else(|| format!("no note with id {id}"))?;
            crate::orchestration::notes::authorize_note(
                &app_ref.edges,
                requested_by,
                id,
                crate::model::EdgeCapability::WriteNote,
            )?;
            app_ref.parse_file_argument(raw)?.0
        };
        if path.is_root() {
            return Err("a note needs a file path, not the project root".to_string());
        }
        App::set_note_backing(
            app,
            id,
            Some(NoteFileBacking {
                path: path.display().to_string(),
                revision: None,
            }),
        );
        App::sync_note(app, id);
        Ok(path.display().to_string())
    }

    /// Turns a file-backed note back into an internal one (its current
    /// content is kept; the file is left untouched).
    pub fn detach_note_file(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
    ) -> Result<(), String> {
        {
            let app_ref = app.borrow();
            app_ref
                .nodes
                .get(&id)
                .and_then(|entry| entry.record.as_note())
                .ok_or_else(|| format!("no note with id {id}"))?;
            crate::orchestration::notes::authorize_note(
                &app_ref.edges,
                requested_by,
                id,
                crate::model::EdgeCapability::WriteNote,
            )?;
        }
        App::set_note_backing(app, id, None);
        Ok(())
    }

    fn set_note_backing(app: &Rc<RefCell<App>>, id: Uuid, backing: Option<NoteFileBacking>) {
        let note_node = {
            let mut app_mut = app.borrow_mut();
            app_mut.file_sync.remove(&id);
            let Some(entry) = app_mut.nodes.get_mut(&id) else {
                return;
            };
            if let Some(note) = entry.record.as_note_mut() {
                note.file = backing.clone();
            }
            match &entry.widget {
                NodeWidget::Note(node) => Some(node.clone()),
                _ => None,
            }
        };
        if let Some(node) = note_node {
            node.set_file_backing(backing.as_ref().map(|b| b.path.as_str()));
            node.set_sync_status(if backing.is_some() { "linking…" } else { "" });
        }
        App::schedule_persist(app);
    }

    /// One pass of file synchronization, run on a timer from `main.rs`:
    /// every file-backed Note is reconciled with its file (local edits
    /// written, external edits loaded, conflicts surfaced — see
    /// `project::sync::reconcile`), every editor notices external changes,
    /// and every few ticks FileTrees refresh their listing and Git markers.
    pub fn sync_project_files(app: &Rc<RefCell<App>>) {
        let (notes, editors, trees, tick) = {
            let mut app_mut = app.borrow_mut();
            app_mut.sync_tick = app_mut.sync_tick.wrapping_add(1);
            let mut notes = Vec::new();
            let mut editors = Vec::new();
            let mut trees = Vec::new();
            for (id, entry) in &app_mut.nodes {
                match &entry.record.kind {
                    NodeKind::Note(note) if note.file.is_some() => notes.push(*id),
                    NodeKind::Editor(editor) if editor.diff.is_none() => editors.push(*id),
                    NodeKind::FileTree(_) => trees.push(*id),
                    _ => {}
                }
            }
            (notes, editors, trees, app_mut.sync_tick)
        };
        for id in notes {
            App::sync_note(app, id);
        }
        for id in editors {
            App::sync_editor(app, id);
        }
        if tick % TREE_REFRESH_TICKS == 0 {
            for id in trees {
                let searching = app
                    .borrow()
                    .tree_state
                    .get(&id)
                    .is_some_and(|state| !state.query.trim().is_empty());
                if !searching {
                    App::refresh_file_tree(app, id);
                }
            }
        }
    }

    fn note_widget(&self, id: Uuid) -> Option<NoteNode> {
        match &self.nodes.get(&id)?.widget {
            NodeWidget::Note(node) => Some(node.clone()),
            _ => None,
        }
    }

    /// Reconciles one file-backed note with its file.
    pub fn sync_note(app: &Rc<RefCell<App>>, id: Uuid) {
        let (project, backing, markdown, last_seen, banner) = {
            let app_ref = app.borrow();
            let Some(note) = app_ref.nodes.get(&id).and_then(|e| e.record.as_note()) else {
                return;
            };
            let Some(backing) = note.file.clone() else {
                return;
            };
            let state = app_ref.file_sync.get(&id);
            (
                app_ref.project(),
                backing,
                note.markdown.clone(),
                state.and_then(|s| s.last_seen),
                state.and_then(|s| s.banner),
            )
        };
        let Some(node) = app.borrow().note_widget(id) else {
            return;
        };
        let path = match ProjectPath::parse(&backing.path) {
            Ok(path) => path,
            Err(error) => {
                node.set_sync_status("error");
                node.container.set_tooltip_text(Some(&error.to_string()));
                return;
            }
        };
        let local = FileRevision::of(markdown.as_bytes());
        let current_signature = signature(&project, &path);
        let local_unchanged = backing.revision.as_ref() == Some(&local);
        if local_unchanged && banner.is_none() && last_seen == Some(current_signature) {
            return;
        }
        let disk = match disk_state(&project, &path) {
            Ok(disk) => disk,
            Err(error) => {
                node.set_sync_status("error");
                node.container.set_tooltip_text(Some(&error.to_string()));
                return;
            }
        };
        let action = reconcile(backing.revision.as_ref(), &local, disk.revision());
        let new_base = match action {
            SyncAction::InSync => backing.revision.clone(),
            SyncAction::Converged => disk.revision().cloned(),
            SyncAction::WriteLocal => {
                match crate::project::sync::write_checked(
                    &project,
                    &path,
                    disk.revision(),
                    markdown.as_bytes(),
                ) {
                    Ok(revision) => Some(revision),
                    // Changed again between our read and our write: the next
                    // tick sees it and decides afresh.
                    Err(_) => backing.revision.clone(),
                }
            }
            SyncAction::LoadDisk => {
                let DiskState::Present { content, revision } = &disk else {
                    return;
                };
                match String::from_utf8(content.clone()) {
                    Ok(text) => {
                        App::set_note_markdown(app, id, text);
                        Some(revision.clone())
                    }
                    Err(_) => {
                        node.set_sync_status("not text");
                        return;
                    }
                }
            }
            SyncAction::Conflict(kind) => {
                let banner = match kind {
                    ConflictKind::BothChanged => SyncBanner::NoteConflict,
                    ConflictKind::DeletedExternally => SyncBanner::NoteDeleted,
                };
                app.borrow_mut().file_sync.entry(id).or_default().banner = Some(banner);
                node.set_sync_status("conflict");
                match banner {
                    SyncBanner::NoteDeleted => node.show_banner(
                        &format!("{path} was deleted on disk."),
                        "Recreate file",
                        "Unlink note",
                    ),
                    _ => node.show_banner(
                        &format!("{path} changed on disk and this note has different edits."),
                        "Use file version",
                        "Keep mine",
                    ),
                }
                return;
            }
        };
        {
            let mut app_mut = app.borrow_mut();
            let state = app_mut.file_sync.entry(id).or_default();
            state.banner = None;
            state.last_seen = Some(signature(&project, &path));
            if let Some(note) = app_mut
                .nodes
                .get_mut(&id)
                .and_then(|e| e.record.as_note_mut())
                && let Some(file) = note.file.as_mut()
                && file.revision != new_base
            {
                file.revision = new_base;
            }
        }
        if banner.is_some() {
            node.hide_banner();
        }
        node.set_sync_status(match action {
            SyncAction::WriteLocal => "saved",
            SyncAction::LoadDisk => "reloaded",
            _ => "synced",
        });
        node.container.set_tooltip_text(None);
        if action != SyncAction::InSync {
            App::schedule_persist(app);
        }
    }

    /// A note banner's buttons: `primary` is "Use file version" / "Recreate
    /// file", the other is "Keep mine" / "Unlink note".
    fn resolve_note_banner(app: &Rc<RefCell<App>>, id: Uuid, primary: bool) {
        let banner = app.borrow().file_sync.get(&id).and_then(|s| s.banner);
        let Some(banner) = banner else { return };
        let (project, backing, markdown) = {
            let app_ref = app.borrow();
            let Some(note) = app_ref.nodes.get(&id).and_then(|e| e.record.as_note()) else {
                return;
            };
            let Some(backing) = note.file.clone() else {
                return;
            };
            (app_ref.project(), backing, note.markdown.clone())
        };
        let Ok(path) = ProjectPath::parse(&backing.path) else {
            return;
        };
        let new_base = match (banner, primary) {
            (SyncBanner::NoteConflict, true) => match disk_state(&project, &path) {
                Ok(DiskState::Present { content, revision }) => match String::from_utf8(content) {
                    Ok(text) => {
                        App::set_note_markdown(app, id, text);
                        Some(revision)
                    }
                    Err(_) => return,
                },
                _ => return,
            },
            // "Keep mine" and "Recreate file" are the user explicitly
            // choosing to overwrite whatever is (or isn't) on disk.
            (SyncBanner::NoteConflict, false) | (SyncBanner::NoteDeleted, true) => {
                if project.write(&path, markdown.as_bytes()).is_err() {
                    return;
                }
                Some(FileRevision::of(markdown.as_bytes()))
            }
            (SyncBanner::NoteDeleted, false) => {
                App::set_note_backing(app, id, None);
                return;
            }
            _ => return,
        };
        {
            let mut app_mut = app.borrow_mut();
            app_mut.file_sync.entry(id).or_default().banner = None;
            if let Some(file) = app_mut
                .nodes
                .get_mut(&id)
                .and_then(|e| e.record.as_note_mut())
                .and_then(|note| note.file.as_mut())
            {
                file.revision = new_base;
            }
        }
        let node = app.borrow().note_widget(id);
        if let Some(node) = node {
            node.hide_banner();
            node.set_sync_status("synced");
        }
        App::schedule_persist(app);
    }

    fn editor_widget(&self, id: Uuid) -> Option<(EditorNode, EditorPayload)> {
        let entry = self.nodes.get(&id)?;
        match (&entry.widget, &entry.record.kind) {
            (NodeWidget::Editor(node), NodeKind::Editor(payload)) => {
                Some((node.clone(), payload.clone()))
            }
            _ => None,
        }
    }

    /// (Re)loads an editor from disk — or, for a diff view, from Git —
    /// discarding any unsaved buffer content.
    pub fn load_editor(app: &Rc<RefCell<App>>, id: Uuid) {
        let Some((node, payload)) = app.borrow().editor_widget(id) else {
            return;
        };
        let project = app.borrow().project();
        node.hide_banner();
        let path = match ProjectPath::parse(&payload.path) {
            Ok(path) => path,
            Err(error) => {
                node.show_message(&error.to_string());
                return;
            }
        };
        node.diff_button.set_visible(payload.diff.is_none());
        node.source_button.set_visible(payload.diff.is_some());
        if let Some(scope) = payload.diff {
            node.set_read_only(true);
            match GitService::new(&project).diff(Some(&path), scope) {
                Ok(diff) if diff.is_empty() => {
                    node.set_text("");
                    node.show_message(&format!("No {} changes in {path}.", scope_label(scope)));
                }
                Ok(diff) => {
                    node.set_text(&diff);
                    node.configure_language(None, Some("diff"));
                }
                Err(error) => node.show_message(&error.to_string()),
            }
            node.set_status(scope_label(scope));
            return;
        }
        let (base, status) = match disk_state(&project, &path) {
            Ok(DiskState::Present { content, revision }) => {
                let class =
                    crate::project::fs::classify(&path, &content[..content.len().min(8192)]);
                match class {
                    FileClass::Image => match node.show_image(&content) {
                        Ok(()) => (Some(revision), ""),
                        Err(error) => {
                            node.show_message(&format!("Can't display this image: {error}"));
                            (Some(revision), "")
                        }
                    },
                    FileClass::Binary => {
                        node.show_message(&format!(
                            "{path} is a binary file and can't be edited here.\nReference it as {}.",
                            file_reference(&path, None)
                        ));
                        node.set_read_only(true);
                        (Some(revision), "binary")
                    }
                    FileClass::Markdown | FileClass::Text => match String::from_utf8(content) {
                        Ok(text) => {
                            node.set_text(&text);
                            node.configure_language(Some(path.file_name()), None);
                            (Some(revision), "")
                        }
                        // Editing a lossy decoding and saving it would
                        // silently rewrite the undecodable bytes.
                        Err(_) => {
                            node.show_message(&format!(
                                "{path} is not valid UTF-8, so it can't be edited safely here."
                            ));
                            node.set_read_only(true);
                            (Some(revision), "not UTF-8")
                        }
                    },
                }
            }
            Ok(DiskState::Missing) => {
                node.show_message(&format!("{path} doesn't exist (any more)."));
                (None, "missing")
            }
            Err(error) => {
                node.show_message(&error.to_string());
                (None, "error")
            }
        };
        node.set_status(status);
        let mut app_mut = app.borrow_mut();
        let state = app_mut.file_sync.entry(id).or_default();
        state.base = base;
        state.banner = None;
        state.last_seen = Some(signature(&project, &path));
    }

    /// Ctrl+S / Save: writes the buffer only if the file is still at the
    /// revision it was loaded from; otherwise offers Overwrite / Reload.
    pub fn save_editor(app: &Rc<RefCell<App>>, id: Uuid, force: bool) {
        let Some((node, payload)) = app.borrow().editor_widget(id) else {
            return;
        };
        if payload.diff.is_some() || node.display() != EditorDisplay::Source {
            return;
        }
        let Ok(path) = ProjectPath::parse(&payload.path) else {
            return;
        };
        let (project, base) = {
            let app_ref = app.borrow();
            (
                app_ref.project(),
                app_ref.file_sync.get(&id).and_then(|s| s.base.clone()),
            )
        };
        let text = node.text();
        let result = if force {
            project
                .write(&path, text.as_bytes())
                .map(|()| FileRevision::of(text.as_bytes()))
                .map_err(|error| error.to_string())
        } else {
            crate::project::sync::write_checked(&project, &path, base.as_ref(), text.as_bytes())
                .map_err(|error| match error {
                    crate::project::sync::WriteError::Conflict { .. } => String::new(),
                    other => other.to_string(),
                })
        };
        match result {
            Ok(revision) => {
                node.mark_saved();
                node.hide_banner();
                node.set_status("saved");
                {
                    let mut app_mut = app.borrow_mut();
                    let state = app_mut.file_sync.entry(id).or_default();
                    state.base = Some(revision);
                    state.banner = None;
                    state.last_seen = Some(signature(&project, &path));
                }
                App::refresh_file_trees(app);
                App::refresh_diff_views_for(app, &path);
            }
            Err(message) if message.is_empty() => {
                app.borrow_mut().file_sync.entry(id).or_default().banner =
                    Some(SyncBanner::EditorSaveConflict);
                node.set_status("conflict");
                node.show_banner(
                    &format!("{path} changed on disk since you opened it."),
                    "Overwrite file",
                    Some("Reload from disk"),
                );
            }
            Err(message) => {
                node.set_status("error");
                node.show_banner(&format!("Couldn't save: {message}"), "Dismiss", None);
            }
        }
    }

    fn refresh_diff_views_for(app: &Rc<RefCell<App>>, path: &ProjectPath) {
        let ids: Vec<Uuid> = app
            .borrow()
            .nodes
            .iter()
            .filter(|(_, entry)| {
                matches!(&entry.record.kind, NodeKind::Editor(e)
                    if e.diff.is_some() && ProjectPath::parse(&e.path).is_ok_and(|p| path.starts_with(&p)))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            App::load_editor(app, id);
        }
    }

    /// Notices external changes to an editor's file: an unmodified buffer
    /// silently follows the file; a modified one gets a banner instead.
    fn sync_editor(app: &Rc<RefCell<App>>, id: Uuid) {
        let Some((node, payload)) = app.borrow().editor_widget(id) else {
            return;
        };
        let Ok(path) = ProjectPath::parse(&payload.path) else {
            return;
        };
        let (project, base, last_seen, banner) = {
            let app_ref = app.borrow();
            let state = app_ref.file_sync.get(&id);
            (
                app_ref.project(),
                state.and_then(|s| s.base.clone()),
                state.and_then(|s| s.last_seen),
                state.and_then(|s| s.banner),
            )
        };
        let current_signature = signature(&project, &path);
        if last_seen == Some(current_signature) {
            return;
        }
        let Ok(disk) = disk_state(&project, &path) else {
            return;
        };
        app.borrow_mut().file_sync.entry(id).or_default().last_seen = Some(current_signature);
        if disk.revision() == base.as_ref() {
            return;
        }
        if banner.is_some() {
            // Already asking the user something; don't stack prompts.
            return;
        }
        match disk {
            DiskState::Missing => {
                app.borrow_mut().file_sync.entry(id).or_default().banner =
                    Some(SyncBanner::EditorDeleted);
                node.set_status("deleted");
                node.show_banner(
                    &format!("{path} was deleted on disk."),
                    "Save to recreate",
                    Some("Dismiss"),
                );
            }
            DiskState::Present { .. } if !node.is_modified() => {
                App::load_editor(app, id);
                node.set_status("reloaded");
            }
            DiskState::Present { .. } => {
                app.borrow_mut().file_sync.entry(id).or_default().banner =
                    Some(SyncBanner::EditorChangedWhileDirty);
                node.set_status("conflict");
                node.show_banner(
                    &format!("{path} changed on disk while you have unsaved edits."),
                    "Reload from disk",
                    Some("Keep my edits"),
                );
            }
        }
    }

    fn resolve_editor_banner(app: &Rc<RefCell<App>>, id: Uuid, primary: bool) {
        let banner = app.borrow().file_sync.get(&id).and_then(|s| s.banner);
        let Some((node, _)) = app.borrow().editor_widget(id) else {
            return;
        };
        match (banner, primary) {
            (Some(SyncBanner::EditorChangedWhileDirty), true)
            | (Some(SyncBanner::EditorSaveConflict), false) => {
                App::load_editor(app, id);
            }
            (Some(SyncBanner::EditorSaveConflict), true)
            | (Some(SyncBanner::EditorDeleted), true) => {
                App::save_editor(app, id, true);
            }
            // "Keep my edits" / "Dismiss": leave the buffer alone. The base
            // revision is unchanged, so a later save still detects the
            // conflict and asks again rather than clobbering the file.
            _ => {
                if let Some(state) = app.borrow_mut().file_sync.get_mut(&id) {
                    state.banner = None;
                }
                node.hide_banner();
            }
        }
    }

    /// Re-renders every FileTree in the active workspace.
    pub fn refresh_file_trees(app: &Rc<RefCell<App>>) {
        let ids: Vec<Uuid> = app
            .borrow()
            .nodes
            .iter()
            .filter(|(_, entry)| matches!(entry.record.kind, NodeKind::FileTree(_)))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(state) = app.borrow_mut().tree_state.get_mut(&id) {
                state.files = None;
            }
            App::refresh_file_tree(app, id);
        }
    }

    fn tree_widget(&self, id: Uuid) -> Option<(FileTreeNode, FileTreePayload)> {
        let entry = self.nodes.get(&id)?;
        match (&entry.widget, &entry.record.kind) {
            (NodeWidget::FileTree(node), NodeKind::FileTree(payload)) => {
                Some((node.clone(), payload.clone()))
            }
            _ => None,
        }
    }

    /// Recomputes one FileTree's rows (tree, filename hits, or content
    /// hits, depending on its search field) and pushes them to the widget
    /// if they changed.
    pub fn refresh_file_tree(app: &Rc<RefCell<App>>, id: Uuid) {
        let Some((node, payload)) = app.borrow().tree_widget(id) else {
            return;
        };
        let (project, query, cached, workspace_name, can_back, can_forward) = {
            let app_ref = app.borrow();
            let state = app_ref.tree_state.get(&id);
            (
                app_ref.project(),
                state.map(|s| s.query.clone()).unwrap_or_default(),
                state.and_then(|s| s.files.clone()),
                app_ref
                    .workspace_root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| app_ref.workspace_root.display().to_string()),
                state.is_some_and(|s| s.history.can_go_back()),
                state.is_some_and(|s| s.history.can_go_forward()),
            )
        };
        let root = parse_root(&payload.root);
        let query = query.trim().to_string();
        let mut new_cache = None;
        let (items, status) = if query.is_empty() {
            tree_items(&project, &root, &payload)
        } else if let Some(pattern) = query.strip_prefix('>') {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                (
                    vec![FileTreeItem::Message(
                        "Type a pattern to search file contents.".to_string(),
                    )],
                    String::new(),
                )
            } else {
                match search_content(
                    &project,
                    pattern,
                    &ContentSearchOptions {
                        root: root.clone(),
                        include_hidden: payload.show_hidden,
                        respect_gitignore: payload.respect_gitignore,
                        fixed_strings: false,
                        limit: TREE_SEARCH_LIMIT,
                    },
                ) {
                    Ok(result) => {
                        let status = format!(
                            "{}{} matches ({})",
                            result.matches.len(),
                            if result.truncated { "+" } else { "" },
                            result.engine
                        );
                        let mut items: Vec<FileTreeItem> = result
                            .matches
                            .into_iter()
                            .map(|hit| FileTreeItem::ContentHit {
                                path: hit.path.display().to_string(),
                                line: hit.line,
                                text: hit.text,
                            })
                            .collect();
                        if items.is_empty() {
                            items.push(FileTreeItem::Message("No matches.".to_string()));
                        }
                        (items, status)
                    }
                    Err(error) => (vec![FileTreeItem::Message(error)], String::new()),
                }
            }
        } else {
            let files = match cached {
                Some(files) => Ok(files),
                None => list_files(
                    &project,
                    &root,
                    ListOptions {
                        include_hidden: payload.show_hidden,
                        respect_gitignore: payload.respect_gitignore,
                        ..ListOptions::default()
                    },
                ),
            };
            match files {
                Ok(files) => {
                    let hits = fuzzy_filter(&query, &files, TREE_SEARCH_LIMIT);
                    let status = format!("{} of {} files", hits.len(), files.len());
                    let mut items: Vec<FileTreeItem> = hits
                        .into_iter()
                        .map(|hit| FileTreeItem::NameHit {
                            path: hit.path.display().to_string(),
                            positions: hit.positions,
                        })
                        .collect();
                    if items.is_empty() {
                        items.push(FileTreeItem::Message("No matching files.".to_string()));
                    }
                    new_cache = Some(files);
                    (items, status)
                }
                Err(error) => (vec![FileTreeItem::Message(error)], String::new()),
            }
        };
        if let Some(files) = new_cache
            && let Some(state) = app.borrow_mut().tree_state.get_mut(&id)
        {
            state.files = Some(files);
        }
        let title = if !payload.root_label.is_empty() && root.is_root() {
            payload.root_label.clone()
        } else if root.is_root() {
            workspace_name
        } else {
            format!("{workspace_name}/{root}")
        };
        node.title_label.set_text(&title);
        node.title_label.set_tooltip_text(Some(
            &project.root().join(root.as_str()).display().to_string(),
        ));
        node.status_label.set_text(&status);
        node.back_button.set_sensitive(can_back);
        node.forward_button.set_sensitive(can_forward);
        node.up_button.set_sensitive(!root.is_root());
        node.hidden_toggle.set_active(payload.show_hidden);
        node.gitignore_toggle.set_active(payload.respect_gitignore);
        if node.items() != items {
            node.set_items(items, payload.selected.as_deref());
        }
    }

    /// Edits a FileTree's persisted payload, then refreshes it.
    fn update_tree(app: &Rc<RefCell<App>>, id: Uuid, edit: impl FnOnce(&mut FileTreePayload)) {
        {
            let mut app_mut = app.borrow_mut();
            let Some(entry) = app_mut.nodes.get_mut(&id) else {
                return;
            };
            let NodeKind::FileTree(payload) = &mut entry.record.kind else {
                return;
            };
            edit(payload);
            if let Some(state) = app_mut.tree_state.get_mut(&id) {
                state.files = None;
            }
        }
        App::schedule_persist(app);
        App::refresh_file_tree(app, id);
    }

    /// Re-roots a FileTree at `root`, recording history for back/forward.
    fn navigate_tree(app: &Rc<RefCell<App>>, id: Uuid, root: ProjectPath) {
        let current = match app.borrow().tree_widget(id) {
            Some((_, payload)) => parse_root(&payload.root),
            None => return,
        };
        if current == root {
            return;
        }
        if let Some(state) = app.borrow_mut().tree_state.get_mut(&id) {
            state.history.navigate(current);
        }
        App::update_tree(app, id, |payload| payload.root = root.as_str().to_string());
    }

    fn step_tree_history(app: &Rc<RefCell<App>>, id: Uuid, forward: bool) {
        let Some((_, payload)) = app.borrow().tree_widget(id) else {
            return;
        };
        let current = parse_root(&payload.root);
        let next = {
            let mut app_mut = app.borrow_mut();
            let Some(state) = app_mut.tree_state.get_mut(&id) else {
                return;
            };
            if forward {
                state.history.forward(current)
            } else {
                state.history.back(current)
            }
        };
        if let Some(next) = next {
            App::update_tree(app, id, |payload| payload.root = next.as_str().to_string());
        }
    }

    /// Where a node opened *from* `source` should appear: to its right.
    fn beside(app: &Rc<RefCell<App>>, source: Uuid) -> (f64, f64) {
        let app_ref = app.borrow();
        let Some(entry) = app_ref.nodes.get(&source) else {
            return (120.0, 120.0);
        };
        let opened = app_ref
            .nodes
            .values()
            .filter(|e| matches!(e.record.kind, NodeKind::Editor(_) | NodeKind::Note(_)))
            .count() as f64;
        (
            entry.record.position.0 + entry.record.size.0 + 40.0 + (opened % 6.0) * 24.0,
            entry.record.position.1 + (opened % 6.0) * 24.0,
        )
    }

    /// `ProjectPath` for an absolute host path, if it's inside the root.
    pub fn project_path_for(&self, absolute: &std::path::Path) -> Option<ProjectPath> {
        self.parse_file_argument(&absolute.to_string_lossy())
            .ok()
            .map(|(path, _)| path)
    }
}

fn scope_label(scope: DiffScope) -> &'static str {
    match scope {
        DiffScope::Unstaged => "unstaged diff",
        DiffScope::Staged => "staged diff",
        DiffScope::Head => "uncommitted diff",
    }
}

/// The tree rows for a FileTree with no search query, plus its status line.
fn tree_items(
    project: &LocalProject,
    root: &ProjectPath,
    payload: &FileTreePayload,
) -> (Vec<FileTreeItem>, String) {
    let git = GitService::new(project);
    let status = payload.show_git_status.then(|| git.status().ok()).flatten();
    let expanded: BTreeSet<ProjectPath> = payload
        .expanded
        .iter()
        .filter_map(|raw| ProjectPath::parse(raw).ok())
        .collect();
    let options = TreeOptions {
        show_hidden: payload.show_hidden,
        respect_gitignore: payload.respect_gitignore,
    };
    match build_tree(project, root, &expanded, options, status.as_ref()) {
        Ok(view) => {
            let mut items: Vec<FileTreeItem> = view
                .rows
                .into_iter()
                .map(|row| FileTreeItem::Entry {
                    path: row.path.display().to_string(),
                    name: row.name,
                    depth: row.depth,
                    is_dir: row.kind == EntryKind::Directory,
                    expanded: row.expanded,
                    marker: row.marker,
                    unreachable_link: row.kind == EntryKind::Symlink,
                })
                .collect();
            if items.is_empty() {
                items.push(FileTreeItem::Message("Empty directory.".to_string()));
            }
            if view.truncated {
                items.push(FileTreeItem::Message(
                    "…more entries not shown.".to_string(),
                ));
            }
            let summary = match &status {
                Some(status) => format!(
                    "{} · {} changed",
                    status.branch.as_deref().unwrap_or("detached"),
                    status.entries.len()
                ),
                None if payload.show_git_status => "not a Git repository".to_string(),
                None => String::new(),
            };
            (items, summary)
        }
        Err(error) => (vec![FileTreeItem::Message(error)], String::new()),
    }
}

/// Builds and wires a FileTree node's widget — called by
/// `materialize_node` before the node's entry exists, so the first render
/// happens on idle, once it does.
pub(super) fn materialize_file_tree(
    app: &Rc<RefCell<App>>,
    id: Uuid,
    collapsed: bool,
    toast_overlay: &adw::ToastOverlay,
) -> NodeWidget {
    let node = FileTreeNode::new(collapsed, {
        let app = Rc::clone(app);
        move |collapsed| {
            if let Some(entry) = app.borrow_mut().nodes.get_mut(&id) {
                entry.record.collapsed = collapsed;
            }
            App::schedule_persist(&app);
        }
    });
    app.borrow_mut().tree_state.insert(id, TreeState::default());

    node.connect_toggle({
        let app = Rc::clone(app);
        move |item| toggle_tree_item(&app, id, &item)
    });
    node.connect_select({
        let app = Rc::clone(app);
        move |item| {
            if let Some(path) = item.path() {
                let path = path.to_string();
                if let Some(entry) = app.borrow_mut().nodes.get_mut(&id)
                    && let NodeKind::FileTree(payload) = &mut entry.record.kind
                {
                    payload.selected = Some(path);
                }
                App::schedule_persist(&app);
            }
        }
    });
    node.connect_activate({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        move |item| activate_tree_item(&app, id, &item, &toast_overlay)
    });
    node.connect_context_menu({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        move |item, anchor| show_tree_menu(&app, id, item, &anchor, &toast_overlay)
    });
    node.refresh_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| {
            if let Some(state) = app.borrow_mut().tree_state.get_mut(&id) {
                state.files = None;
            }
            App::refresh_file_tree(&app, id);
        }
    });
    node.collapse_all_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::update_tree(&app, id, |payload| payload.expanded.clear())
    });
    node.up_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| {
            let parent = app
                .borrow()
                .tree_widget(id)
                .and_then(|(_, payload)| parse_root(&payload.root).parent());
            if let Some(parent) = parent {
                App::navigate_tree(&app, id, parent);
            }
        }
    });
    node.back_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::step_tree_history(&app, id, false)
    });
    node.forward_button.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::step_tree_history(&app, id, true)
    });
    node.hidden_toggle.connect_toggled({
        let app = Rc::clone(app);
        move |toggle| {
            let active = toggle.is_active();
            let changed = app
                .borrow()
                .tree_widget(id)
                .is_some_and(|(_, payload)| payload.show_hidden != active);
            if changed {
                App::update_tree(&app, id, |payload| payload.show_hidden = active);
            }
        }
    });
    node.gitignore_toggle.connect_toggled({
        let app = Rc::clone(app);
        move |toggle| {
            let active = toggle.is_active();
            let changed = app
                .borrow()
                .tree_widget(id)
                .is_some_and(|(_, payload)| payload.respect_gitignore != active);
            if changed {
                App::update_tree(&app, id, |payload| payload.respect_gitignore = active);
            }
        }
    });
    node.search_entry.connect_search_changed({
        let app = Rc::clone(app);
        move |entry| {
            let query = entry.text().to_string();
            if let Some(state) = app.borrow_mut().tree_state.get_mut(&id) {
                state.query = query;
            }
            App::refresh_file_tree(&app, id);
        }
    });
    node.search_entry.connect_activate({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        let node = node.clone();
        move |_| {
            // Enter opens the top hit.
            if let Some(item) = node.items().into_iter().find(|item| item.path().is_some()) {
                activate_tree_item(&app, id, &item, &toast_overlay);
            }
        }
    });
    node.commit_button.connect_clicked({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        let anchor = node.container.clone();
        move |_| open_commit_dialog(&app, &anchor, &toast_overlay)
    });
    {
        let app = Rc::clone(app);
        glib::idle_add_local_once(move || App::refresh_file_tree(&app, id));
    }
    NodeWidget::FileTree(node)
}

fn toggle_tree_item(app: &Rc<RefCell<App>>, id: Uuid, item: &FileTreeItem) {
    let FileTreeItem::Entry {
        path, is_dir: true, ..
    } = item
    else {
        return;
    };
    let path = path.clone();
    App::update_tree(app, id, |payload| {
        if !payload.expanded.remove(&path) {
            payload.expanded.insert(path.clone());
        }
    });
}

fn activate_tree_item(
    app: &Rc<RefCell<App>>,
    id: Uuid,
    item: &FileTreeItem,
    toast_overlay: &adw::ToastOverlay,
) {
    if item.is_dir() {
        toggle_tree_item(app, id, item);
        return;
    }
    let Some(raw) = item.path() else { return };
    let Ok(path) = ProjectPath::parse(raw) else {
        return;
    };
    let line = match item {
        FileTreeItem::ContentHit { line, .. } => Some(*line as u32),
        _ => None,
    };
    if let FileTreeItem::NameHit { .. } | FileTreeItem::ContentHit { .. } = item {
        // Reveal the hit in the tree for when the search is cleared. (Bound
        // first: an `if let` scrutinee's `app.borrow()` would otherwise stay
        // alive through the `borrow_mut` below.)
        let tree = app.borrow().tree_widget(id);
        if let Some((_, payload)) = tree {
            let root = parse_root(&payload.root);
            let mut expanded: BTreeSet<ProjectPath> = payload
                .expanded
                .iter()
                .filter_map(|raw| ProjectPath::parse(raw).ok())
                .collect();
            reveal(&mut expanded, &root, &path);
            if let Some(entry) = app.borrow_mut().nodes.get_mut(&id)
                && let NodeKind::FileTree(payload) = &mut entry.record.kind
            {
                payload.expanded = expanded.iter().map(|p| p.as_str().to_string()).collect();
                payload.selected = Some(path.as_str().to_string());
            }
        }
    }
    let position = App::beside(app, id);
    if let Err(error) = App::open_project_file(app, &path, position, line) {
        toast(toast_overlay, &error);
    }
}

/// One popover-menu entry: its label and what clicking it does.
type MenuItem = (String, Box<dyn Fn()>);

/// Fills a node's reusable `popover` with a menu of `(label, action)`
/// items and shows it. Popovers are owned by their node and never
/// unparented (see `node_files::FileTreeNode::menu_popover`).
fn popup_menu(popover: &gtk4::Popover, items: Vec<MenuItem>) {
    let list = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    for (label, action) in items {
        let button = gtk4::Button::with_label(&label);
        button.add_css_class("flat");
        if let Some(child) = button.child().and_downcast::<gtk4::Label>() {
            child.set_xalign(0.0);
        }
        let action: Rc<dyn Fn()> = Rc::from(action);
        button.connect_clicked({
            let popover = popover.clone();
            move |_| {
                popover.popdown();
                let action = Rc::clone(&action);
                // After the popover is gone, so an action that opens a
                // dialog or rebuilds the tree doesn't fight the popdown.
                glib::idle_add_local_once(move || action());
            }
        });
        list.append(&button);
    }
    popover.set_child(Some(&list));
    popover.popup();
}

fn show_tree_menu(
    app: &Rc<RefCell<App>>,
    id: Uuid,
    item: FileTreeItem,
    anchor: &gtk4::Widget,
    toast_overlay: &adw::ToastOverlay,
) {
    let Some(raw) = item.path() else { return };
    let Ok(path) = ProjectPath::parse(raw) else {
        return;
    };
    let is_dir = item.is_dir();
    let status = app.borrow().git_status().ok();
    let entry_status = status.as_ref().and_then(|s| s.entry(&path).cloned());
    let has_changes = status.as_ref().is_some_and(|s| s.has_changes_under(&path));
    let Some((tree_node, _)) = app.borrow().tree_widget(id) else {
        return;
    };
    let mut items: Vec<MenuItem> = Vec::new();
    let add = |items: &mut Vec<MenuItem>, label: &str, action: Box<dyn Fn()>| {
        items.push((label.to_string(), action));
    };

    if is_dir {
        let (app_c, path_c) = (Rc::clone(app), path.clone());
        add(
            &mut items,
            "Open as tree root",
            Box::new(move || App::navigate_tree(&app_c, id, path_c.clone())),
        );
    } else {
        let (app_c, path_c, toast_c) = (Rc::clone(app), path.clone(), toast_overlay.clone());
        add(
            &mut items,
            "Open",
            Box::new(move || {
                let position = App::beside(&app_c, id);
                if let Err(error) = App::open_project_file(&app_c, &path_c, position, None) {
                    toast(&toast_c, &error);
                }
            }),
        );
        if path
            .extension()
            .is_some_and(|ext| matches!(ext.as_str(), "md" | "markdown" | "mdown" | "mkd"))
        {
            let (app_c, path_c, toast_c) = (Rc::clone(app), path.clone(), toast_overlay.clone());
            add(
                &mut items,
                "Open in editor",
                Box::new(move || {
                    let position = App::beside(&app_c, id);
                    if let Err(error) = App::open_editor(&app_c, &path_c, None, position, None) {
                        toast(&toast_c, &error);
                    }
                }),
            );
        }
    }
    if has_changes {
        let (app_c, path_c, toast_c) = (Rc::clone(app), path.clone(), toast_overlay.clone());
        add(
            &mut items,
            "Show diff",
            Box::new(move || {
                let position = App::beside(&app_c, id);
                if let Err(error) =
                    App::open_editor(&app_c, &path_c, Some(DiffScope::Head), position, None)
                {
                    toast(&toast_c, &error);
                }
            }),
        );
    }
    {
        let (path_c, anchor_c, toast_c) = (path.clone(), anchor.clone(), toast_overlay.clone());
        add(
            &mut items,
            "Copy reference",
            Box::new(move || {
                let reference = file_reference(&path_c, None);
                anchor_c.clipboard().set_text(&reference);
                toast(&toast_c, &format!("Copied {reference}"));
            }),
        );
    }
    {
        let (app_c, path_c, toast_c) = (Rc::clone(app), path.clone(), toast_overlay.clone());
        let ask_popover = tree_node.aux_popover.clone();
        add(
            &mut items,
            "Ask an agent about this…",
            Box::new(move || {
                ask_agent(
                    &app_c,
                    &ask_popover,
                    format!("Please take a look at {}", file_reference(&path_c, None)),
                    &toast_c,
                );
            }),
        );
        if has_changes {
            let (app_c, path_c, toast_c) = (Rc::clone(app), path.clone(), toast_overlay.clone());
            let ask_popover = tree_node.aux_popover.clone();
            add(
                &mut items,
                "Send diff to an agent…",
                Box::new(move || {
                    ask_agent(
                        &app_c,
                        &ask_popover,
                        format!(
                            "Please review the uncommitted changes in {} (resolve it with `duetctl resolve`, read it with `duetctl git diff {}`)",
                            diff_reference(&path_c),
                            path_c
                        ),
                        &toast_c,
                    );
                }),
            );
        }
    }
    let unstaged = entry_status
        .as_ref()
        .map(|e| e.has_unstaged())
        .unwrap_or(is_dir && has_changes);
    let staged = entry_status
        .as_ref()
        .map(|e| e.has_staged())
        .unwrap_or(is_dir && has_changes);
    if unstaged {
        let (app_c, raw_c, toast_c) = (
            Rc::clone(app),
            path.display().to_string(),
            toast_overlay.clone(),
        );
        add(
            &mut items,
            "Stage",
            Box::new(move || {
                if let Err(error) = App::git_stage(&app_c, std::slice::from_ref(&raw_c)) {
                    toast(&toast_c, &error);
                }
            }),
        );
    }
    if staged {
        let (app_c, raw_c, toast_c) = (
            Rc::clone(app),
            path.display().to_string(),
            toast_overlay.clone(),
        );
        add(
            &mut items,
            "Unstage",
            Box::new(move || {
                if let Err(error) = App::git_unstage(&app_c, std::slice::from_ref(&raw_c)) {
                    toast(&toast_c, &error);
                }
            }),
        );
    }
    if unstaged && !entry_status.as_ref().is_some_and(|e| e.is_untracked()) {
        let (app_c, path_c, anchor_c, toast_c) = (
            Rc::clone(app),
            path.clone(),
            anchor.clone(),
            toast_overlay.clone(),
        );
        add(
            &mut items,
            "Discard changes…",
            Box::new(move || {
                confirm_discard(&app_c, &path_c, &anchor_c, &toast_c);
            }),
        );
    }
    tree_node.point_menu_at(anchor);
    popup_menu(&tree_node.menu_popover, items);
}

fn confirm_discard(
    app: &Rc<RefCell<App>>,
    path: &ProjectPath,
    anchor: &gtk4::Widget,
    toast_overlay: &adw::ToastOverlay,
) {
    let dialog = adw::MessageDialog::new(
        window_of(anchor).as_ref(),
        Some(&format!("Discard changes to {path}?")),
        Some("Unstaged changes will be permanently lost. Staged changes are kept."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("discard", "Discard");
    dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, {
        let app = Rc::clone(app);
        let raw = path.display().to_string();
        let toast_overlay = toast_overlay.clone();
        move |_, response| {
            if response == "discard" {
                match App::git_discard(&app, std::slice::from_ref(&raw), true) {
                    Ok(()) => toast(&toast_overlay, &format!("Discarded changes to {raw}")),
                    Err(error) => toast(&toast_overlay, &error),
                }
            }
        }
    });
    dialog.present();
}

fn open_commit_dialog(
    app: &Rc<RefCell<App>>,
    anchor: &impl IsA<gtk4::Widget>,
    toast_overlay: &adw::ToastOverlay,
) {
    let status = match app.borrow().git_status() {
        Ok(status) => status,
        Err(error) => {
            toast(toast_overlay, &error);
            return;
        }
    };
    let staged: Vec<String> = status
        .entries
        .iter()
        .filter(|e| e.has_staged())
        .map(|e| format!("{} {}", e.index, e.path))
        .collect();
    if staged.is_empty() {
        toast(
            toast_overlay,
            "Nothing is staged. Stage files from the tree's right-click menu first.",
        );
        return;
    }
    let dialog = adw::MessageDialog::new(
        window_of(anchor).as_ref(),
        Some("Commit staged changes"),
        Some(&staged.join("\n")),
    );
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Commit message"));
    entry.set_activates_default(true);
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("commit", "Commit");
    dialog.set_response_appearance("commit", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("commit"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, {
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        let entry = entry.clone();
        move |_, response| {
            if response == "commit" {
                match App::git_commit(&app, &entry.text()) {
                    Ok(hash) => toast(&toast_overlay, &format!("Committed {hash}")),
                    Err(error) => toast(&toast_overlay, &error),
                }
            }
        }
    });
    dialog.present();
    entry.grab_focus();
}

/// "Ask an agent about this": a popover with an editable message
/// (pre-filled with a stable `@file:`/`@diff:` reference, never pasted
/// content) and one button per agent in the workspace. Sends through the
/// same `App::send_message` the CLI uses, as the human operator.
pub(super) fn ask_agent(
    app: &Rc<RefCell<App>>,
    popover: &gtk4::Popover,
    message: String,
    toast_overlay: &adw::ToastOverlay,
) {
    let agents = app.borrow().agent_infos();
    if agents.is_empty() {
        toast(
            toast_overlay,
            "There are no agents in this workspace to ask.",
        );
        return;
    }
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    let entry = gtk4::Entry::new();
    entry.set_text(&message);
    entry.set_width_chars(48);
    content.append(&gtk4::Label::new(Some(
        "Message (the reference resolves through Duet):",
    )));
    content.append(&entry);
    for agent in agents {
        let label = match &agent.role {
            Some(role) => format!("Send to {} ({role})", agent.name),
            None => format!("Send to {}", agent.name),
        };
        let button = gtk4::Button::with_label(&label);
        button.connect_clicked({
            let app = Rc::clone(app);
            let entry = entry.clone();
            let popover = popover.clone();
            let toast_overlay = toast_overlay.clone();
            move |_| {
                popover.popdown();
                match App::send_message(&app, None, &agent.id.to_string(), entry.text().to_string())
                {
                    Ok(_) => toast(&toast_overlay, &format!("Sent to {}", agent.name)),
                    Err(error) => toast(&toast_overlay, &error),
                }
            }
        });
        content.append(&button);
    }
    popover.set_child(Some(&content));
    popover.popup();
    entry.grab_focus();
}

/// Builds and wires an Editor node's widget; content is loaded on idle,
/// once the node's entry exists.
pub(super) fn materialize_editor(
    app: &Rc<RefCell<App>>,
    id: Uuid,
    payload: &EditorPayload,
    collapsed: bool,
    toast_overlay: &adw::ToastOverlay,
) -> NodeWidget {
    let node = EditorNode::new(&payload.path, collapsed, {
        let app = Rc::clone(app);
        move |collapsed| {
            if let Some(entry) = app.borrow_mut().nodes.get_mut(&id) {
                entry.record.collapsed = collapsed;
            }
            App::schedule_persist(&app);
        }
    });
    app.borrow_mut()
        .file_sync
        .insert(id, FileSyncState::default());
    node.connect_save({
        let app = Rc::clone(app);
        move || App::save_editor(&app, id, false)
    });
    node.reload_button.connect_clicked({
        let app = Rc::clone(app);
        let node = node.clone();
        move |_| {
            if node.is_modified() {
                node.show_banner(
                    "Reloading discards your unsaved edits.",
                    "Reload anyway",
                    Some("Cancel"),
                );
                app.borrow_mut().file_sync.entry(id).or_default().banner =
                    Some(SyncBanner::EditorChangedWhileDirty);
            } else {
                App::load_editor(&app, id);
            }
        }
    });
    node.banner_primary.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::resolve_editor_banner(&app, id, true)
    });
    node.banner_secondary.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::resolve_editor_banner(&app, id, false)
    });
    let path = ProjectPath::parse(&payload.path).ok();
    let diff = payload.diff;
    node.copy_reference_button.connect_clicked({
        let node = node.clone();
        let path = path.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let Some(path) = &path else { return };
            let reference = editor_reference(&node, path, diff);
            node.container.clipboard().set_text(&reference);
            toast(&toast_overlay, &format!("Copied {reference}"));
        }
    });
    node.ask_agent_button.connect_clicked({
        let app = Rc::clone(app);
        let node = node.clone();
        let path = path.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let Some(path) = &path else { return };
            let reference = editor_reference(&node, path, diff);
            let message = if diff.is_some() {
                format!("Please review the uncommitted changes in {reference} (read them with `duetctl git diff {path}`)")
            } else {
                format!("Please take a look at {reference}")
            };
            ask_agent(&app, &node.aux_popover, message, &toast_overlay);
        }
    });
    node.diff_button.connect_clicked({
        let app = Rc::clone(app);
        let path = path.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let Some(path) = &path else { return };
            let position = App::beside(&app, id);
            if let Err(error) = App::open_editor(&app, path, Some(DiffScope::Head), position, None)
            {
                toast(&toast_overlay, &error);
            }
        }
    });
    node.source_button.connect_clicked({
        let app = Rc::clone(app);
        let path = path.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let Some(path) = &path else { return };
            let position = App::beside(&app, id);
            if let Err(error) = App::open_project_file(&app, path, position, None) {
                toast(&toast_overlay, &error);
            }
        }
    });
    for (button, stage) in [(&node.stage_button, true), (&node.unstage_button, false)] {
        button.connect_clicked({
            let app = Rc::clone(app);
            let path = path.clone();
            let toast_overlay = toast_overlay.clone();
            move |_| {
                let Some(path) = &path else { return };
                let raw = [path.display().to_string()];
                let result = if stage {
                    App::git_stage(&app, &raw)
                } else {
                    App::git_unstage(&app, &raw)
                };
                match result {
                    Ok(()) => toast(
                        &toast_overlay,
                        &format!("{} {path}", if stage { "Staged" } else { "Unstaged" }),
                    ),
                    Err(error) => toast(&toast_overlay, &error),
                }
            }
        });
    }
    {
        let app = Rc::clone(app);
        glib::idle_add_local_once(move || App::load_editor(&app, id));
    }
    NodeWidget::Editor(node)
}

/// The reference the editor's "Copy reference"/"Ask an agent" use: the
/// selected lines of a source view, or the diff itself.
fn editor_reference(node: &EditorNode, path: &ProjectPath, diff: Option<DiffScope>) -> String {
    if diff.is_some() {
        return diff_reference(path);
    }
    if node.has_selection() {
        let (start, end) = node.selected_lines();
        file_reference(path, Some(LineRange { start, end }))
    } else {
        file_reference(path, None)
    }
}

/// Wires a Note node's file-backing UI: the path label, the conflict
/// banner's buttons, and the link-to-file popover. Called for every note
/// (any internal note can be linked later).
pub(super) fn wire_note_file(
    app: &Rc<RefCell<App>>,
    node: &NoteNode,
    id: Uuid,
    backing: Option<&NoteFileBacking>,
    toast_overlay: &adw::ToastOverlay,
) {
    node.set_file_backing(backing.map(|b| b.path.as_str()));
    if backing.is_some() {
        node.set_sync_status("…");
        let app = Rc::clone(app);
        glib::idle_add_local_once(move || App::sync_note(&app, id));
    }
    node.banner_primary.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::resolve_note_banner(&app, id, true)
    });
    node.banner_secondary.connect_clicked({
        let app = Rc::clone(app);
        move |_| App::resolve_note_banner(&app, id, false)
    });
    node.file_button.connect_clicked({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        let popover = node.file_popover.clone();
        move |button| show_note_file_popover(&app, id, button, &popover, &toast_overlay)
    });
}

fn show_note_file_popover(
    app: &Rc<RefCell<App>>,
    id: Uuid,
    anchor: &gtk4::Button,
    popover: &gtk4::Popover,
    toast_overlay: &adw::ToastOverlay,
) {
    let current = app
        .borrow()
        .nodes
        .get(&id)
        .and_then(|e| e.record.as_note())
        .and_then(|note| note.file.clone());
    let anchor_widget: gtk4::Widget = anchor.clone().upcast();
    match current {
        Some(backing) => {
            let mut items: Vec<MenuItem> = Vec::new();
            let (app_c, toast_c) = (Rc::clone(app), toast_overlay.clone());
            let path = backing.path.clone();
            items.push((
                "Copy reference".to_string(),
                Box::new({
                    let anchor = anchor_widget.clone();
                    move || {
                        let reference = format!("@file:{path}");
                        anchor.clipboard().set_text(&reference);
                        toast(&toast_c, &format!("Copied {reference}"));
                    }
                }),
            ));
            let toast_c = toast_overlay.clone();
            items.push((
                "Unlink from file".to_string(),
                Box::new(move || {
                    if let Err(error) = App::detach_note_file(&app_c, None, id) {
                        toast(&toast_c, &error);
                    }
                }),
            ));
            popup_menu(popover, items);
        }
        None => {
            let content = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            content.append(&gtk4::Label::new(Some(
                "Sync this note with a project file:",
            )));
            let entry = gtk4::Entry::new();
            entry.set_placeholder_text(Some("docs/notes.md"));
            entry.set_width_chars(30);
            let link = gtk4::Button::with_label("Link");
            link.add_css_class("suggested-action");
            content.append(&entry);
            content.append(&link);
            popover.set_child(Some(&content));
            let submit = {
                let app = Rc::clone(app);
                let entry = entry.clone();
                let popover = popover.clone();
                let toast_overlay = toast_overlay.clone();
                move || {
                    popover.popdown();
                    match App::attach_note_file(&app, None, id, &entry.text()) {
                        Ok(path) => toast(&toast_overlay, &format!("Note linked to {path}")),
                        Err(error) => toast(&toast_overlay, &error),
                    }
                }
            };
            let submit = Rc::new(submit);
            link.connect_clicked({
                let submit = Rc::clone(&submit);
                move |_| submit()
            });
            entry.connect_activate(move |_| submit());
            popover.popup();
            entry.grab_focus();
        }
    }
}

/// Lets project files be dropped on the canvas — from a FileTree row
/// (whose drag payload is its `@file:` reference) or from a file manager —
/// opening each the way `App::open_project_file` decides. Files outside the
/// project root are refused.
pub fn install_canvas_drop(app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) {
    let target = gtk4::DropTarget::new(glib::Type::INVALID, gtk4::gdk::DragAction::COPY);
    target.set_types(&[String::static_type(), gtk4::gdk::FileList::static_type()]);
    target.connect_drop({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        move |_target, value, x, y| {
            let mut paths: Vec<Result<ProjectPath, String>> = Vec::new();
            if let Ok(text) = value.get::<String>() {
                let Some(body) = text.trim().strip_prefix(FILE_DRAG_PREFIX) else {
                    return false;
                };
                paths.push(
                    split_line_suffix(body)
                        .and_then(|(raw, _)| ProjectPath::parse(raw).map_err(|e| e.to_string())),
                );
            } else if let Ok(list) = value.get::<gtk4::gdk::FileList>() {
                for file in list.files() {
                    let Some(absolute) = file.path() else {
                        continue;
                    };
                    paths.push(app.borrow().project_path_for(&absolute).ok_or_else(|| {
                        format!(
                            "{} is outside this workspace's project root",
                            absolute.display()
                        )
                    }));
                }
            } else {
                return false;
            }
            let world = {
                let app_ref = app.borrow();
                let state = app_ref.canvas.state.borrow();
                canvas::screen_to_world((x, y), state.pan, state.zoom)
            };
            for (index, path) in paths.into_iter().enumerate() {
                let position = (world.0 + index as f64 * 32.0, world.1 + index as f64 * 32.0);
                let result =
                    path.and_then(|path| App::open_project_file(&app, &path, position, None));
                if let Err(error) = result {
                    toast(&toast_overlay, &error);
                }
            }
            true
        }
    });
    app.borrow().canvas.overlay.add_controller(target);
}

/// Forgets the runtime state of a node that's leaving the canvas.
pub(super) fn forget_node(app: &mut App, id: Uuid) {
    app.file_sync.remove(&id);
    app.tree_state.remove(&id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountStore;
    use crate::agent::Agent;
    use crate::model::{EdgeCapability, EdgeRecord, EnvironmentKind, TerminalPayload};
    use crate::project::git::tests::{git, p, repo};
    use crate::store::Store;
    use sourceview5::prelude::*;

    fn test_app(root: &std::path::Path) -> Rc<RefCell<App>> {
        let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
        let app = App::new(
            AccountStore::new(tmp.join("accounts")),
            tmp.join("store.json"),
        );
        app.borrow_mut().workspace_root = root.to_path_buf();
        app
    }

    fn shell_agent(name: &str, cwd: &std::path::Path) -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (480.0, 320.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Terminal(TerminalPayload {
                name: name.to_string(),
                cwd: cwd.to_path_buf(),
                agent: Agent::Shell,
                claude_session_id: None,
                claude_account: None,
                never_launched: false,
                role_id: None,
                environment: EnvironmentKind::LocalPty,
            }),
        }
    }

    fn tree_paths(app: &Rc<RefCell<App>>, id: Uuid) -> Vec<String> {
        let (node, _) = app.borrow().tree_widget(id).unwrap();
        node.items()
            .iter()
            .filter_map(|item| match item {
                FileTreeItem::Entry { path, marker, .. } => Some(match marker {
                    Some(marker) => format!("{path} [{marker}]"),
                    None => path.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    fn note_markdown(app: &Rc<RefCell<App>>, id: Uuid) -> String {
        app.borrow()
            .nodes
            .get(&id)
            .unwrap()
            .record
            .as_note()
            .unwrap()
            .markdown
            .clone()
    }

    /// Writes `content` and makes sure the file's (mtime, size) signature
    /// differs from before, so the polling watcher can't miss it even on a
    /// coarse-mtime filesystem.
    fn external_write(root: &std::path::Path, path: &str, content: &str) {
        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(root.join(path), content).unwrap();
    }

    /// The Milestone 6 acceptance run, driven through the same `App`
    /// services the GUI and `duetctl` call, against real widgets and a real
    /// Git repository: browse, Git status, open/edit/save a source file,
    /// inspect its diff, refer to it as `@file:` from an agent, resolve it,
    /// send it to a Reviewer through the message flow, open a Markdown file
    /// as a Note, edit it externally, and observe sync + conflict handling.
    /// Needs a display: `cargo test acceptance_milestone_6 -- --ignored --exact`.
    #[test]
    #[ignore = "needs a display"]
    fn acceptance_milestone_6_project_files_end_to_end() {
        if gtk4::init().is_err() {
            return;
        }
        let (tmp, project) = repo();
        project
            .write(&p("docs/plan.md"), b"# Plan\n\n- [ ] login\n")
            .unwrap();
        git(tmp.path(), &["add", "-A"]);
        git(tmp.path(), &["commit", "-q", "-m", "docs"]);
        let app = test_app(tmp.path());

        // 1. Browse the repository.
        let tree = App::create_file_tree(&app, (0.0, 0.0)).unwrap();
        App::refresh_file_tree(&app, tree);
        assert_eq!(tree_paths(&app, tree), vec!["docs", "src"]);
        let src_row = app.borrow().tree_widget(tree).unwrap().0.items()[1].clone();
        toggle_tree_item(&app, tree, &src_row);
        assert_eq!(tree_paths(&app, tree), vec!["docs", "src", "src/auth.rs"]);

        // 2. Inspect Git status.
        let status = app.borrow().git_status().unwrap();
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(status.entries.is_empty());

        // 3. Open a source file — it opens in the editor, not as a note.
        let auth = p("src/auth.rs");
        let editor = App::open_project_file(&app, &auth, (400.0, 0.0), None).unwrap();
        App::load_editor(&app, editor);
        let (editor_node, _) = app.borrow().editor_widget(editor).unwrap();
        assert_eq!(editor_node.text(), "fn login() {}\n");
        assert_eq!(
            editor_node
                .buffer
                .language()
                .map(|l| l.id().to_string())
                .as_deref(),
            Some("rust")
        );

        // 4. Edit and save.
        editor_node
            .buffer
            .set_text("fn login() {\n    check_password();\n}\n");
        assert!(editor_node.is_modified());
        App::save_editor(&app, editor, false);
        assert!(!editor_node.is_modified());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/auth.rs")).unwrap(),
            "fn login() {\n    check_password();\n}\n"
        );
        App::refresh_file_tree(&app, tree);
        assert!(tree_paths(&app, tree).contains(&"src/auth.rs [M]".to_string()));
        assert!(tree_paths(&app, tree).contains(&"src [•]".to_string()));

        // 5. Inspect the diff — via the service and in a diff view.
        let diff = app
            .borrow()
            .git_diff(Some("src/auth.rs"), DiffScope::Head)
            .unwrap();
        assert!(diff.contains("+    check_password();"));
        let diff_view =
            App::open_editor(&app, &auth, Some(DiffScope::Head), (800.0, 0.0), None).unwrap();
        App::load_editor(&app, diff_view);
        let (diff_node, _) = app.borrow().editor_widget(diff_view).unwrap();
        assert!(diff_node.text().contains("+    check_password();"));
        assert!(!diff_node.view.is_editable());

        // 6-7. An agent prompt refers to it as @file:path; Duet resolves it
        // in the agent's own workspace project.
        let lead = shell_agent("Lead", tmp.path());
        let reviewer = shell_agent("Reviewer", tmp.path());
        let toast_overlay = adw::ToastOverlay::new();
        materialize_node(&app, lead.clone(), &toast_overlay).unwrap();
        materialize_node(&app, reviewer.clone(), &toast_overlay).unwrap();
        app.borrow_mut().edges.push(EdgeRecord {
            id: Uuid::new_v4(),
            source: lead.id,
            target: reviewer.id,
            capabilities: [EdgeCapability::SendMessages].into_iter().collect(),
        });
        let crate::orchestration::ResolveOutcome::Found { resource } = app
            .borrow()
            .resolve_resource("@file:src/auth.rs#L2", Some(reviewer.id))
            .unwrap()
        else {
            panic!("expected @file:src/auth.rs to resolve");
        };
        assert_eq!(resource.kind, ResourceKind::File);
        assert_eq!(resource.path.as_deref(), Some("src/auth.rs"));
        assert_eq!(resource.workspace_id, app.borrow().workspace_id);
        let excerpt = app.borrow().read_file(&resource.reference(), None).unwrap();
        assert_eq!(excerpt.content, "    check_password();\n");
        assert!(matches!(
            app.borrow()
                .resolve_resource("@src/auth.rs", Some(reviewer.id))
                .unwrap(),
            crate::orchestration::ResolveOutcome::Found { .. }
        ));
        assert!(
            app.borrow()
                .resolve_resource("@file:../outside.rs", None)
                .is_err()
        );

        // 8. Send it for review through the existing message flow — a
        // reference, not a pasted diff — and the Reviewer resolves it.
        let message = App::send_message(
            &app,
            Some(lead.id),
            &reviewer.id.to_string(),
            "Please review @diff:src/auth.rs".to_string(),
        )
        .unwrap();
        assert_ne!(message.status, crate::message::DeliveryStatus::Failed);
        let crate::orchestration::ResolveOutcome::Found {
            resource: diff_resource,
        } = app
            .borrow()
            .resolve_resource("@diff:src/auth.rs", Some(reviewer.id))
            .unwrap()
        else {
            panic!("expected @diff:src/auth.rs to resolve");
        };
        assert_eq!(diff_resource.kind, ResourceKind::Diff);
        let reviewed = app
            .borrow()
            .git_diff(diff_resource.path.as_deref(), DiffScope::Head)
            .unwrap();
        assert_eq!(reviewed, diff);

        // 9. Open a Markdown file as a (file-backed) Note.
        let plan = App::open_project_file(&app, &p("docs/plan.md"), (0.0, 600.0), None).unwrap();
        assert_eq!(note_markdown(&app, plan), "# Plan\n\n- [ ] login\n");
        App::sync_note(&app, plan);
        let note_widget = app.borrow().note_widget(plan).unwrap();
        assert!(!note_widget.banner_visible());

        // 10-11. Edit it externally: the note follows.
        external_write(
            tmp.path(),
            "docs/plan.md",
            "# Plan\n\n- [x] login\n- [ ] logout\n",
        );
        App::sync_note(&app, plan);
        assert_eq!(
            note_markdown(&app, plan),
            "# Plan\n\n- [x] login\n- [ ] logout\n"
        );
        assert_eq!(
            crate::node::buffer_text(&note_widget.edit_view.buffer()),
            "# Plan\n\n- [x] login\n- [ ] logout\n"
        );

        // A local (agent) edit is written back to the file.
        App::append_note(&app, None, plan, "- [ ] reset password".to_string()).unwrap();
        App::sync_note(&app, plan);
        let on_disk = std::fs::read_to_string(tmp.path().join("docs/plan.md")).unwrap();
        assert!(on_disk.contains("reset password"));

        // Concurrent edits on both sides: a conflict, never a silent
        // overwrite of the external change.
        App::replace_note(&app, None, plan, "# Plan\n\nmine\n".to_string()).unwrap();
        external_write(tmp.path(), "docs/plan.md", "# Plan\n\ntheirs, longer\n");
        App::sync_note(&app, plan);
        assert!(note_widget.banner_visible());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("docs/plan.md")).unwrap(),
            "# Plan\n\ntheirs, longer\n"
        );
        assert_eq!(note_markdown(&app, plan), "# Plan\n\nmine\n");
        // Further ticks keep waiting for the user.
        App::sync_note(&app, plan);
        assert!(note_widget.banner_visible());
        // The user keeps theirs (the note's): now it's written, explicitly.
        App::resolve_note_banner(&app, plan, false);
        assert!(!note_widget.banner_visible());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("docs/plan.md")).unwrap(),
            "# Plan\n\nmine\n"
        );

        // Everything that should persist does, runtime state doesn't.
        app.borrow().persist().unwrap();
        let store = Store::load(&app.borrow().store_path);
        let workspace = &store.workspaces[0];
        let saved_tree = workspace.nodes.iter().find(|n| n.id == tree).unwrap();
        let NodeKind::FileTree(saved_tree) = &saved_tree.kind else {
            panic!()
        };
        assert!(saved_tree.expanded.contains("src"));
        let saved_note = workspace.nodes.iter().find(|n| n.id == plan).unwrap();
        let backing = saved_note.as_note().unwrap().file.clone().unwrap();
        assert_eq!(backing.path, "docs/plan.md");
        assert_eq!(
            backing.revision,
            Some(FileRevision::of(b"# Plan\n\nmine\n"))
        );
        let saved_editor = workspace.nodes.iter().find(|n| n.id == editor).unwrap();
        assert!(
            matches!(&saved_editor.kind, NodeKind::Editor(e) if e.path == "src/auth.rs" && e.diff.is_none())
        );
    }

    /// An editor follows external changes while clean, and refuses to save
    /// over an external change once it has its own edits.
    #[test]
    #[ignore = "needs a display"]
    fn editor_external_changes_and_save_conflicts() {
        if gtk4::init().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "one\n").unwrap();
        let app = test_app(tmp.path());
        let editor = App::open_editor(&app, &p("a.txt"), None, (0.0, 0.0), None).unwrap();
        App::load_editor(&app, editor);
        let (node, _) = app.borrow().editor_widget(editor).unwrap();

        // Clean buffer: silently reloads.
        external_write(tmp.path(), "a.txt", "two two\n");
        App::sync_editor(&app, editor);
        assert_eq!(node.text(), "two two\n");
        assert!(!node.banner.is_visible());

        // Dirty buffer + external change: banner, nothing written.
        node.buffer.set_text("mine\n");
        external_write(tmp.path(), "a.txt", "three three three\n");
        App::sync_editor(&app, editor);
        assert!(node.banner.is_visible());
        App::save_editor(&app, editor, false);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "three three three\n"
        );
        assert!(node.is_modified());

        // Explicit overwrite.
        App::resolve_editor_banner(&app, editor, true);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "mine\n"
        );
        assert!(!node.is_modified());

        // Binary files are represented, not mangled.
        std::fs::write(tmp.path().join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        assert!(App::open_project_file(&app, &p("blob.bin"), (0.0, 0.0), None).is_err());

        // Text that only turns out not to be UTF-8 past the sniffed prefix
        // is never loaded lossily (a save would rewrite those bytes).
        let mut tricky = vec![b'a'; 9000];
        tricky.push(0xff);
        std::fs::write(tmp.path().join("tricky.txt"), &tricky).unwrap();
        std::fs::write(tmp.path().join("tricky.md"), &tricky).unwrap();
        assert!(App::open_project_file(&app, &p("tricky.md"), (0.0, 0.0), None).is_err());
        let tricky_editor =
            App::open_project_file(&app, &p("tricky.txt"), (0.0, 0.0), None).unwrap();
        App::load_editor(&app, tricky_editor);
        let (tricky_node, _) = app.borrow().editor_widget(tricky_editor).unwrap();
        assert_eq!(tricky_node.display(), EditorDisplay::Message);
        App::save_editor(&app, tricky_editor, false);
        assert_eq!(
            std::fs::read(tmp.path().join("tricky.txt")).unwrap(),
            tricky
        );
    }

    /// Attaching an internal note to an existing, different file reports a
    /// conflict instead of overwriting the file; attaching to a new path
    /// creates it.
    #[test]
    #[ignore = "needs a display"]
    fn attaching_a_note_never_clobbers_an_existing_file() {
        if gtk4::init().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("existing.md"), "# Theirs\n").unwrap();
        let app = test_app(tmp.path());
        let note = App::new_record(
            &app,
            (0.0, 0.0),
            (200.0, 200.0),
            NodeKind::Note(NotePayload {
                markdown: "# Mine\n".to_string(),
                color: "yellow".to_string(),
                view_mode: NoteViewMode::Edit,
                file: None,
            }),
        );
        let id = App::add_file_node(&app, note).unwrap();

        App::attach_note_file(&app, None, id, "existing.md").unwrap();
        assert!(app.borrow().note_widget(id).unwrap().banner_visible());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("existing.md")).unwrap(),
            "# Theirs\n"
        );

        App::detach_note_file(&app, None, id).unwrap();
        App::attach_note_file(&app, None, id, "new/notes.md").unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("new/notes.md")).unwrap(),
            "# Mine\n"
        );
        assert!(App::attach_note_file(&app, None, id, "../escape.md").is_err());
    }

    /// Regression: activating a search hit (double-click / Enter) panicked
    /// with "RefCell already borrowed" — the reveal step held an
    /// `app.borrow()` alive across its own `borrow_mut()`.
    #[test]
    #[ignore = "needs a display"]
    fn activating_search_hits_opens_and_reveals_them() {
        if gtk4::init().is_err() {
            return;
        }
        let (tmp, _project) = repo();
        let app = test_app(tmp.path());
        let tree = App::create_file_tree(&app, (0.0, 0.0)).unwrap();
        let toast_overlay = adw::ToastOverlay::new();
        let name_hit = FileTreeItem::NameHit {
            path: "src/auth.rs".to_string(),
            positions: vec![],
        };
        activate_tree_item(&app, tree, &name_hit, &toast_overlay);
        let content_hit = FileTreeItem::ContentHit {
            path: "src/auth.rs".to_string(),
            line: 1,
            text: "fn login() {}".to_string(),
        };
        activate_tree_item(&app, tree, &content_hit, &toast_overlay);
        let app_ref = app.borrow();
        let editors = app_ref
            .nodes
            .values()
            .filter(|e| matches!(&e.record.kind, NodeKind::Editor(editor) if editor.path == "src/auth.rs"))
            .count();
        assert_eq!(editors, 2);
        let NodeKind::FileTree(payload) = &app_ref.nodes.get(&tree).unwrap().record.kind else {
            panic!("expected a FileTree");
        };
        assert!(payload.expanded.contains("src"));
        assert_eq!(payload.selected.as_deref(), Some("src/auth.rs"));
    }
}
