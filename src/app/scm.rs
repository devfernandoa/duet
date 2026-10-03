//! `App`'s repository-level Git services (Milestone 7.5) and the header's
//! compact Git status control.
//!
//! The per-file operations (status, diff, stage/unstage, discard, commit)
//! live in `files.rs` next to the FileTree that uses them; this module adds
//! the branch- and remote-level workflow — Stage All/Unstage All, branch
//! list/switch/create, fetch, pull, push — as `App` methods that both the GUI
//! and `duetctl git` call. All of the actual Git behavior is in the GTK-free
//! `project::git::GitService`.
//!
//! Fetch, pull and push talk to a remote and can take seconds, so they run
//! on a worker thread (a `LocalProject` is plain data and `Send`) and report
//! back on the main loop; only one of them runs at a time. Everything else
//! is a quick local `git` call made in place.

use super::App;
use crate::project::LocalProject;
use crate::project::git::{
    GitBranch, GitError, GitService, GitStatus, GitSyncOutcome, PushTarget, validate_branch_name,
};
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// A Git operation that talks to a remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOp {
    Fetch,
    Pull,
    /// `set_upstream`: publish a branch that has none yet (`push -u`).
    Push {
        set_upstream: bool,
    },
}

impl RemoteOp {
    pub fn label(self) -> &'static str {
        match self {
            RemoteOp::Fetch => "Fetching",
            RemoteOp::Pull => "Pulling",
            RemoteOp::Push { .. } => "Pushing",
        }
    }

    fn run(self, project: &LocalProject) -> Result<GitSyncOutcome, GitError> {
        let git = GitService::new(project);
        match self {
            RemoteOp::Fetch => git.fetch(),
            RemoteOp::Pull => git.pull(),
            RemoteOp::Push { set_upstream } => git.push(set_upstream),
        }
    }
}

/// Called once with the outcome of a [`RemoteOp`].
pub type RemoteOpDone = Box<dyn FnOnce(Result<GitSyncOutcome, GitError>)>;

impl App {
    /// Stages every change in the project.
    pub fn git_stage_all(app: &Rc<RefCell<App>>) -> Result<(), String> {
        let project = app.borrow().project();
        GitService::new(&project)
            .stage_all()
            .map_err(|error| error.to_string())?;
        App::after_git_change(app);
        Ok(())
    }

    /// Unstages everything, keeping the working tree as it is.
    pub fn git_unstage_all(app: &Rc<RefCell<App>>) -> Result<(), String> {
        let project = app.borrow().project();
        GitService::new(&project)
            .unstage_all()
            .map_err(|error| error.to_string())?;
        App::after_git_change(app);
        Ok(())
    }

    pub fn git_branches(&self) -> Result<Vec<GitBranch>, String> {
        GitService::new(&self.project())
            .branches()
            .map_err(|error| error.to_string())
    }

    /// Switches to an existing local branch — never forced, never stashed;
    /// git's own refusal is returned as is.
    pub fn git_switch_branch(app: &Rc<RefCell<App>>, name: &str) -> Result<(), String> {
        App::refuse_while_busy(app)?;
        let project = app.borrow().project();
        GitService::new(&project)
            .switch_branch(name)
            .map_err(|error| error.to_string())?;
        App::after_git_change(app);
        App::reload_open_files(app);
        Ok(())
    }

    /// Creates a branch from `HEAD` and switches to it.
    pub fn git_create_branch(app: &Rc<RefCell<App>>, name: &str) -> Result<(), String> {
        App::refuse_while_busy(app)?;
        let project = app.borrow().project();
        GitService::new(&project)
            .create_branch(name.trim())
            .map_err(|error| error.to_string())?;
        App::after_git_change(app);
        Ok(())
    }

    /// The remote operation in progress, if any.
    pub fn git_busy(&self) -> Option<RemoteOp> {
        self.git_busy
    }

    fn refuse_while_busy(app: &Rc<RefCell<App>>) -> Result<(), String> {
        match app.borrow().git_busy {
            Some(op) => Err(format!(
                "{} is still running; try again when it finishes",
                op.label()
            )),
            None => Ok(()),
        }
    }

    /// Runs `op` on a worker thread and calls `done` on the main loop with
    /// its outcome. Refuses (calling `done` with the reason) while another
    /// remote operation is running, so a double click can't start two.
    /// Afterwards the Git state everywhere is refreshed, whatever happened.
    pub fn git_remote(app: &Rc<RefCell<App>>, op: RemoteOp, done: RemoteOpDone) {
        if let Err(error) = App::refuse_while_busy(app) {
            done(Err(GitError::Invalid(error)));
            return;
        }
        let project = {
            let mut app_mut = app.borrow_mut();
            app_mut.git_busy = Some(op);
            app_mut.project()
        };
        App::git_state_changed(app);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(op.run(&project));
        });
        let app = Rc::clone(app);
        let mut done = Some(done);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let outcome = match receiver.try_recv() {
                Ok(outcome) => outcome,
                Err(std::sync::mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(GitError::Failed(
                    "the Git worker stopped unexpectedly".to_string(),
                )),
            };
            app.borrow_mut().git_busy = None;
            App::after_git_change(&app);
            if op == RemoteOp::Pull {
                App::reload_open_files(&app);
            }
            if let Some(done) = done.take() {
                done(outcome);
            }
            glib::ControlFlow::Break
        });
    }

    /// Registers a callback for "Git state may have changed" — after any
    /// Duet Git operation, a save, or a remote operation starting/finishing.
    /// The header's status button refreshes itself through this.
    pub fn connect_git_state_changed(&mut self, f: impl Fn() + 'static) {
        self.git_listeners.push(Rc::new(f));
    }

    pub(crate) fn git_state_changed(app: &Rc<RefCell<App>>) {
        let listeners = app.borrow().git_listeners.clone();
        for listener in listeners {
            listener();
        }
    }

    /// After a branch switch or pull rewrote files: let every editor and
    /// file-backed note pick the new contents up now rather than on the
    /// next sync tick (a dirty one keeps its edits and shows its banner, as
    /// for any external change).
    fn reload_open_files(app: &Rc<RefCell<App>>) {
        App::sync_project_files(app);
    }
}

/// What the header's Git button shows for `status`: the label text and
/// its tooltip. Pure, so the format is testable without GTK.
pub fn indicator_text(
    status: &Result<GitStatus, String>,
    busy: Option<RemoteOp>,
) -> (String, String) {
    match status {
        Ok(status) => {
            let label = match busy {
                Some(op) => format!("{} {}…", status.summary(), op.label().to_lowercase()),
                None => status.summary(),
            };
            let mut tooltip = format!(
                "Branch {}",
                status.branch.as_deref().unwrap_or("(detached HEAD)")
            );
            match &status.upstream {
                Some(upstream) if status.upstream_gone => {
                    tooltip.push_str(&format!("\nUpstream {upstream} no longer exists"));
                }
                Some(upstream) => tooltip.push_str(&format!(
                    "\n{} ahead, {} behind {upstream}",
                    status.ahead, status.behind
                )),
                None => tooltip.push_str("\nNo upstream branch"),
            }
            if status.is_clean() {
                tooltip.push_str("\nWorking tree clean");
            } else {
                tooltip.push_str(&format!(
                    "\n{} changed, {} staged",
                    status.entries.len(),
                    status.staged_count()
                ));
            }
            (label, tooltip)
        }
        Err(_) => (
            "No Git".to_string(),
            "This workspace's folder is not a Git repository".to_string(),
        ),
    }
}

/// The header's Git control: `main ↑2 ↓1 •3`, opening a popover with the
/// repository's state and the everyday operations. Refreshes itself on
/// every Git state change Duet makes and every few seconds for changes
/// made outside Duet.
pub fn build_git_button(
    app: &Rc<RefCell<App>>,
    window: &impl IsA<gtk4::Window>,
) -> gtk4::MenuButton {
    let label = gtk4::Label::new(None);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    label.set_max_width_chars(22);
    let spinner = gtk4::Spinner::new();
    spinner.set_visible(false);
    let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    content.append(&branch_glyph());
    content.append(&spinner);
    content.append(&label);
    let button = gtk4::MenuButton::new();
    button.set_child(Some(&content));
    button.add_css_class("flat");
    button.add_css_class("git-status-button");
    let popover = gtk4::Popover::new();
    popover.add_css_class("git-popover");
    button.set_popover(Some(&popover));

    let refresh: Rc<dyn Fn()> = Rc::new({
        let app = Rc::clone(app);
        let label = label.clone();
        let spinner = spinner.clone();
        let button = button.clone();
        move || {
            let (status, busy) = {
                let app_ref = app.borrow();
                (app_ref.git_status(), app_ref.git_busy())
            };
            let (text, tooltip) = indicator_text(&status, busy);
            label.set_text(&text);
            button.set_tooltip_text(Some(&tooltip));
            spinner.set_visible(busy.is_some());
            spinner.set_spinning(busy.is_some());
            if status.is_err() {
                button.add_css_class("dim-label");
            } else {
                button.remove_css_class("dim-label");
            }
        }
    });
    refresh();
    app.borrow_mut().connect_git_state_changed({
        let refresh = Rc::clone(&refresh);
        let app = Rc::clone(app);
        let popover = popover.clone();
        let window = window.clone().upcast::<gtk4::Window>();
        move || {
            refresh();
            if popover.is_visible() {
                fill_git_popover(&app, &popover, &window);
            }
        }
    });
    // External changes (a commit in a terminal, an agent's edit): a cheap
    // `git status` every few seconds.
    glib::timeout_add_local(Duration::from_secs(3), {
        let refresh = Rc::clone(&refresh);
        let button = button.clone();
        move || {
            if button.is_mapped() {
                refresh();
            }
            glib::ControlFlow::Continue
        }
    });
    popover.connect_show({
        let app = Rc::clone(app);
        let window = window.clone().upcast::<gtk4::Window>();
        move |popover| {
            fill_git_popover(&app, popover, &window);
        }
    });
    button
}

/// A small branch symbol (the icon theme has none), drawn in the
/// button's own text color so it follows light/dark themes.
fn branch_glyph() -> gtk4::DrawingArea {
    let glyph = gtk4::DrawingArea::new();
    glyph.set_content_width(12);
    glyph.set_content_height(16);
    glyph.set_valign(gtk4::Align::Center);
    glyph.set_draw_func(|area, cr, _width, _height| {
        let color = area.color();
        cr.set_source_rgba(
            color.red() as f64,
            color.green() as f64,
            color.blue() as f64,
            color.alpha() as f64,
        );
        cr.set_line_width(1.6);
        // Trunk, and a branch curving off it.
        cr.move_to(3.5, 4.0);
        cr.line_to(3.5, 12.0);
        let _ = cr.stroke();
        cr.move_to(9.0, 5.5);
        cr.curve_to(9.0, 9.0, 3.5, 8.0, 3.5, 11.0);
        let _ = cr.stroke();
        for (x, y) in [(3.5, 2.8), (3.5, 13.2), (9.0, 4.2)] {
            cr.arc(x, y, 1.9, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    });
    glyph
}

fn heading(text: &str) -> gtk4::Label {
    let label = gtk4::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label
}

fn dim(text: &str) -> gtk4::Label {
    let label = gtk4::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_max_width_chars(40);
    label.add_css_class("dim-label");
    label
}

/// (Re)builds the Git popover from the repository's current state.
fn fill_git_popover(app: &Rc<RefCell<App>>, popover: &gtk4::Popover, window: &gtk4::Window) {
    let (status, branches, busy) = {
        let app_ref = app.borrow();
        (
            app_ref.git_status(),
            app_ref.git_branches(),
            app_ref.git_busy(),
        )
    };
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);
    content.set_width_request(300);

    let status = match status {
        Ok(status) => status,
        Err(error) => {
            content.append(&heading("Not a Git repository"));
            content.append(&dim(&format!(
                "{error}.\nOpen a workspace whose folder is a repository (or run `git init` in it) to use Git here."
            )));
            popover.set_child(Some(&content));
            return;
        }
    };

    // Where am I: branch, upstream, ahead/behind, working tree.
    content.append(&heading(
        status.branch.as_deref().unwrap_or("Detached HEAD"),
    ));
    let tracking = match (&status.upstream, status.upstream_gone) {
        (Some(upstream), true) => format!("Tracks {upstream}, which no longer exists"),
        (Some(upstream), false) if status.ahead == 0 && status.behind == 0 => {
            format!("Up to date with {upstream}")
        }
        (Some(upstream), false) => format!(
            "{} ahead, {} behind {upstream}",
            status.ahead, status.behind
        ),
        (None, _) => "No upstream branch".to_string(),
    };
    content.append(&dim(&tracking));
    let changes = if status.is_clean() {
        "Working tree clean".to_string()
    } else {
        let mut parts = vec![format!("{} changed", status.entries.len())];
        if status.staged_count() > 0 {
            parts.push(format!("{} staged", status.staged_count()));
        }
        if status.conflict_count() > 0 {
            parts.push(format!("{} in conflict", status.conflict_count()));
        }
        parts.join(" · ")
    };
    content.append(&dim(&changes));

    if let Some(op) = busy {
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let spinner = gtk4::Spinner::new();
        spinner.set_spinning(true);
        row.append(&spinner);
        row.append(&dim(&format!("{}…", op.label())));
        content.append(&row);
    }

    // Remote operations.
    let remote_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    remote_row.set_homogeneous(true);
    for (label, tooltip, op) in [
        (
            "Fetch",
            "Download new commits from the remote (changes nothing locally)",
            RemoteOp::Fetch,
        ),
        (
            "Pull",
            "Fast-forward to the upstream; never merges or rebases",
            RemoteOp::Pull,
        ),
        (
            "Push",
            "Send your commits to the upstream; never forces",
            RemoteOp::Push {
                set_upstream: false,
            },
        ),
    ] {
        let button = gtk4::Button::with_label(label);
        button.set_tooltip_text(Some(tooltip));
        let has_upstream = status.upstream.is_some() && !status.upstream_gone;
        button.set_sensitive(busy.is_none() && (op != RemoteOp::Pull || has_upstream));
        if op == RemoteOp::Pull && !has_upstream {
            button.set_tooltip_text(Some("This branch has no upstream to pull from"));
        }
        button.connect_clicked({
            let app = Rc::clone(app);
            let popover = popover.clone();
            let window = window.clone();
            move |_| {
                popover.popdown();
                run_remote_op(&app, op, &window);
            }
        });
        remote_row.append(&button);
    }
    content.append(&remote_row);

    // Local changes.
    let changes_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    changes_row.set_homogeneous(true);
    let commit = gtk4::Button::with_label("Commit…");
    commit.add_css_class("suggested-action");
    commit.set_sensitive(status.staged_count() > 0);
    commit.set_tooltip_text(Some(if status.staged_count() > 0 {
        "Commit the staged changes"
    } else {
        "Nothing is staged yet"
    }));
    commit.connect_clicked({
        let app = Rc::clone(app);
        let popover = popover.clone();
        let window = window.clone();
        move |_| {
            popover.popdown();
            super::files::open_commit_dialog(&app, &window, &App::toaster(&app));
        }
    });
    let stage_all = gtk4::Button::with_label("Stage All");
    stage_all.set_sensitive(status.unstaged_count() > 0);
    stage_all.connect_clicked({
        let app = Rc::clone(app);
        move |_| {
            if let Err(error) = App::git_stage_all(&app) {
                App::notify(&app, &error);
            }
        }
    });
    let unstage_all = gtk4::Button::with_label("Unstage All");
    unstage_all.set_sensitive(status.staged_count() > 0);
    unstage_all.connect_clicked({
        let app = Rc::clone(app);
        move |_| {
            if let Err(error) = App::git_unstage_all(&app) {
                App::notify(&app, &error);
            }
        }
    });
    changes_row.append(&stage_all);
    changes_row.append(&unstage_all);
    content.append(&changes_row);
    content.append(&commit);
    if !status.is_clean() {
        let show = gtk4::Button::with_label("Show all changes");
        show.add_css_class("flat");
        show.connect_clicked({
            let app = Rc::clone(app);
            let popover = popover.clone();
            let window = window.clone();
            move |_| {
                popover.popdown();
                let position = viewport_center(&app, &window);
                if let Err(error) = App::open_editor(
                    &app,
                    &crate::project::path::ProjectPath::root(),
                    Some(crate::project::git::DiffScope::Head),
                    position,
                    None,
                ) {
                    App::notify(&app, &error);
                }
            }
        });
        content.append(&show);
    }

    // Branches.
    content.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    content.append(&heading("Branches"));
    match branches {
        Ok(branches) => {
            let list = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            list.add_css_class("card-menu");
            for branch in branches {
                let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
                let mark = gtk4::Image::from_icon_name("object-select-symbolic");
                mark.set_opacity(if branch.current { 1.0 } else { 0.0 });
                row.append(&mark);
                let name = gtk4::Label::new(Some(&branch.name));
                name.set_xalign(0.0);
                name.set_hexpand(true);
                name.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
                row.append(&name);
                if let Some(upstream) = &branch.upstream {
                    let up = gtk4::Label::new(Some(upstream));
                    up.add_css_class("dim-label");
                    up.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
                    up.set_max_width_chars(16);
                    row.append(&up);
                }
                let button = gtk4::Button::new();
                button.set_child(Some(&row));
                button.add_css_class("flat");
                button.set_sensitive(busy.is_none());
                if branch.current {
                    button.set_tooltip_text(Some("The current branch"));
                } else {
                    button.set_tooltip_text(Some(&format!("Switch to {}", branch.name)));
                    button.connect_clicked({
                        let app = Rc::clone(app);
                        let name = branch.name.clone();
                        move |_| match App::git_switch_branch(&app, &name) {
                            Ok(()) => App::notify(&app, &format!("Switched to {name}")),
                            Err(error) => {
                                show_git_error(&app, &format!("Couldn't switch to {name}"), &error)
                            }
                        }
                    });
                }
                list.append(&button);
            }
            let scroller = gtk4::ScrolledWindow::new();
            scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
            scroller.set_propagate_natural_height(true);
            scroller.set_max_content_height(220);
            scroller.set_child(Some(&list));
            content.append(&scroller);
        }
        Err(error) => content.append(&dim(&error)),
    }

    // New branch.
    let new_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("New branch from here"));
    entry.set_hexpand(true);
    let create = gtk4::Button::with_label("Create");
    create.set_sensitive(false);
    entry.connect_changed({
        let create = create.clone();
        move |entry| {
            let text = entry.text();
            let verdict = validate_branch_name(text.trim());
            create.set_sensitive(verdict.is_ok() && busy.is_none());
            match verdict {
                Err(reason) if !text.is_empty() => {
                    entry.add_css_class("error");
                    entry.set_tooltip_text(Some(&reason));
                }
                _ => {
                    entry.remove_css_class("error");
                    entry.set_tooltip_text(None);
                }
            }
        }
    });
    let do_create: Rc<dyn Fn()> = Rc::new({
        let app = Rc::clone(app);
        let entry = entry.clone();
        let create = create.clone();
        move || {
            if !create.is_sensitive() {
                return;
            }
            let name = entry.text().trim().to_string();
            match App::git_create_branch(&app, &name) {
                Ok(()) => App::notify(&app, &format!("Created and switched to {name}")),
                Err(error) => show_git_error(&app, &format!("Couldn't create {name}"), &error),
            }
        }
    });
    create.connect_clicked({
        let do_create = Rc::clone(&do_create);
        move |_| do_create()
    });
    entry.connect_activate(move |_| do_create());
    new_row.append(&entry);
    new_row.append(&create);
    content.append(&new_row);

    popover.set_child(Some(&content));
}

fn viewport_center(app: &Rc<RefCell<App>>, window: &gtk4::Window) -> (f64, f64) {
    let (width, height) = (window.width().max(1) as f64, window.height().max(1) as f64);
    let app_ref = app.borrow();
    let state = app_ref.canvas.state.borrow();
    crate::canvas::screen_to_world((width / 2.0, height / 2.0), state.pan, state.zoom)
}

/// Starts a remote operation from the GUI and reports the outcome: a toast
/// on success, a dialog with git's reason on failure, and — for a branch
/// with no upstream — an explicit question before publishing it.
pub fn run_remote_op(app: &Rc<RefCell<App>>, op: RemoteOp, window: &gtk4::Window) {
    let app_c = Rc::clone(app);
    let window_c = window.clone();
    App::git_remote(
        app,
        op,
        Box::new(move |outcome| match outcome {
            Ok(outcome) => App::notify(&app_c, &outcome.message),
            Err(GitError::UpstreamRequired(target)) => {
                confirm_publish(&app_c, &target, &window_c);
            }
            Err(error) => {
                let title = match op {
                    RemoteOp::Fetch => "Fetch failed",
                    RemoteOp::Pull => "Pull didn't run",
                    RemoteOp::Push { .. } => "Push didn't run",
                };
                show_git_error(&app_c, title, &error.to_string());
            }
        }),
    );
}

fn confirm_publish(app: &Rc<RefCell<App>>, target: &PushTarget, window: &gtk4::Window) {
    let dialog = adw::MessageDialog::new(
        Some(window),
        Some(&format!("Publish {}?", target.branch)),
        Some(&format!(
            "{} has no upstream branch yet. Push it to {}/{} and track it from now on?",
            target.branch, target.remote, target.branch
        )),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("publish", "Publish Branch");
    dialog.set_response_appearance("publish", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("publish"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, {
        let app = Rc::clone(app);
        let window = window.clone();
        move |_, response| {
            if response == "publish" {
                run_remote_op(&app, RemoteOp::Push { set_upstream: true }, &window);
            }
        }
    });
    dialog.present();
}

/// A Git failure the user has to act on: shown in a dialog (git's reason
/// can be long and must be readable), not a fleeting toast.
pub fn show_git_error(app: &Rc<RefCell<App>>, title: &str, reason: &str) {
    let window = app
        .borrow()
        .canvas
        .overlay
        .root()
        .and_downcast::<gtk4::Window>();
    let Some(window) = window else {
        App::notify(app, &format!("{title}: {reason}"));
        return;
    };
    let dialog = adw::MessageDialog::new(Some(&window), Some(title), Some(reason));
    dialog.add_response("ok", "OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");
    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountStore;
    use crate::project::git::GitStatusEntry;
    use crate::project::path::ProjectPath;
    use std::path::Path;
    use std::time::Instant;

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    fn identity(dir: &Path) {
        git(dir, &["config", "user.name", "Duet Test"]);
        git(dir, &["config", "user.email", "duet@example.invalid"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn spin_until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let context = glib::MainContext::default();
        while !done() {
            assert!(Instant::now() < deadline, "timed out");
            if !context.iteration(false) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Runs one remote op through `App::git_remote` and waits for it.
    fn remote(app: &Rc<RefCell<App>>, op: RemoteOp) -> Result<GitSyncOutcome, GitError> {
        let slot = Rc::new(RefCell::new(None));
        App::git_remote(app, op, {
            let slot = Rc::clone(&slot);
            Box::new(move |outcome| *slot.borrow_mut() = Some(outcome))
        });
        spin_until(|| slot.borrow().is_some());
        slot.take().unwrap()
    }

    /// The App-level Git workflow the header popover and `duetctl git`
    /// share: stage all, commit, branch, publish, fetch/pull on a worker
    /// thread with the UI loop still running, one remote op at a time, and
    /// listeners told about every change. Against a local bare remote.
    #[test]
    #[ignore = "needs a display"]
    fn app_git_workflow_against_a_local_remote() {
        if gtk4::init().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("remote.git");
        git(
            tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                remote_dir.to_str().unwrap(),
            ],
        );
        let work = tmp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        identity(&work);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "initial"]);
        git(
            &work,
            &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        );
        git(&work, &["push", "-q", "-u", "origin", "main"]);

        let state = tmp.path().join("state");
        let app = App::new(
            AccountStore::new(state.join("accounts")),
            state.join("store.json"),
        );
        app.borrow_mut().workspace_root = work.clone();
        let notified = Rc::new(std::cell::Cell::new(0));
        app.borrow_mut().connect_git_state_changed({
            let notified = Rc::clone(&notified);
            move || notified.set(notified.get() + 1)
        });

        std::fs::write(work.join("a.txt"), "two\n").unwrap();
        std::fs::write(work.join("b.txt"), "new\n").unwrap();
        App::git_stage_all(&app).unwrap();
        assert_eq!(app.borrow().git_status().unwrap().staged_count(), 2);
        App::git_unstage_all(&app).unwrap();
        assert_eq!(app.borrow().git_status().unwrap().staged_count(), 0);
        App::git_stage_all(&app).unwrap();
        App::git_commit(&app, "two").unwrap();
        assert!(notified.get() >= 4, "{}", notified.get());
        assert_eq!(app.borrow().git_status().unwrap().ahead, 1);

        let pushed = remote(
            &app,
            RemoteOp::Push {
                set_upstream: false,
            },
        )
        .unwrap();
        assert!(pushed.message.contains("Pushed 1 commit"));
        assert!(app.borrow().git_busy().is_none());

        App::git_create_branch(&app, "feature").unwrap();
        assert!(App::git_create_branch(&app, "bad name").is_err());
        assert!(matches!(
            remote(
                &app,
                RemoteOp::Push {
                    set_upstream: false
                }
            ),
            Err(GitError::UpstreamRequired(_))
        ));
        remote(&app, RemoteOp::Push { set_upstream: true }).unwrap();
        assert_eq!(
            app.borrow().git_status().unwrap().upstream.as_deref(),
            Some("origin/feature")
        );
        App::git_switch_branch(&app, "main").unwrap();
        let branches = app.borrow().git_branches().unwrap();
        assert!(branches.iter().any(|b| b.name == "main" && b.current));

        // While one remote operation runs, another is refused, as are
        // branch switches.
        let first = Rc::new(RefCell::new(None));
        App::git_remote(&app, RemoteOp::Fetch, {
            let first = Rc::clone(&first);
            Box::new(move |outcome| *first.borrow_mut() = Some(outcome))
        });
        assert_eq!(app.borrow().git_busy(), Some(RemoteOp::Fetch));
        let second = Rc::new(RefCell::new(None));
        App::git_remote(&app, RemoteOp::Pull, {
            let second = Rc::clone(&second);
            Box::new(move |outcome| *second.borrow_mut() = Some(outcome))
        });
        assert!(
            matches!(&*second.borrow(), Some(Err(GitError::Invalid(m))) if m.contains("still running"))
        );
        assert!(App::git_switch_branch(&app, "feature").is_err());
        spin_until(|| first.borrow().is_some());
        assert!(first.borrow().as_ref().unwrap().is_ok());
        assert!(app.borrow().git_busy().is_none());
        let pulled = remote(&app, RemoteOp::Pull).unwrap();
        assert!(pulled.message.contains("up to date"), "{pulled:?}");
    }

    #[test]
    fn indicator_shows_branch_tracking_and_changes() {
        let mut status = GitStatus {
            branch: Some("main".to_string()),
            upstream: Some("origin/main".to_string()),
            ahead: 2,
            behind: 1,
            ..GitStatus::default()
        };
        status.entries.push(GitStatusEntry {
            path: ProjectPath::parse("a.rs").unwrap(),
            original_path: None,
            index: 'M',
            worktree: ' ',
        });
        let (label, tooltip) = indicator_text(&Ok(status.clone()), None);
        assert_eq!(label, "main ↑2 ↓1 •1");
        assert!(tooltip.contains("2 ahead, 1 behind origin/main"));
        assert!(tooltip.contains("1 changed, 1 staged"));

        let (busy, _) = indicator_text(&Ok(status), Some(RemoteOp::Fetch));
        assert!(busy.ends_with("fetching…"));

        let (none, tooltip) = indicator_text(&Err("not a repo".to_string()), None);
        assert_eq!(none, "No Git");
        assert!(tooltip.contains("not a Git repository"));
    }
}
