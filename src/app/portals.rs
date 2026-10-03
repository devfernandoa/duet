//! `App`'s Portal service (Milestone 8) — the `PortalService` CLAUDE.md
//! asks for — and the GTK wiring of the Portal card.
//!
//! Every operation here is called identically by the Portal card's own
//! buttons (as the human operator, `requested_by = None`) and by
//! `control.rs` for `duetctl portal ...` (with the acting agent's id), so
//! GUI and agents can never diverge. What a portal *is* and who may do
//! what to it is decided in the GTK-free `orchestration::portal`
//! (`authorize_portal`/`authorize_script`, URL validation, page scripts);
//! the live `WebView`s are owned by `portal_runtime::PortalRuntime`; this
//! file finds a portal's record (in the active workspace or a dormant one
//! — a portal's view outlives its card exactly like a terminal's PTY does),
//! checks authorization, and turns WebKit's callbacks into one `done`
//! callback per operation. Operations that need the page (navigation,
//! text, screenshots, interaction) are asynchronous for that reason;
//! `control.rs` answers the socket from the callback.
//!
//! Borrow discipline as in the rest of `app`: read what's needed in a
//! short borrow, drop it, *then* call into WebKit — loading a URL can emit
//! signals whose handlers borrow `App` again (they defer to an idle
//! callback for exactly that reason).

use super::{
    App, CanvasCommand, MenuItem, NodeWidget, check_label, materialize_node, next_z_order,
    separator,
};
use crate::message::{PortalAction, PortalInfo, PortalPage, PortalScreenshot, PortalSummary};
use crate::model::{
    EdgeCapability, EdgeRecord, FloorRef, NodeKind, NodeRecord, PortalPayload, PortalStorage,
};
use crate::node_portal::PortalNode;
use crate::orchestration::portal::{
    self as domain, DEFAULT_TEXT_LIMIT, EvaluateForm, SCREENSHOTS_KEPT_PER_PORTAL,
    authorize_portal, authorize_script, normalize_url, parse_script_reply, portals_connected_to,
};
use crate::orchestration::resource::{ResolveOutcome, ResourceKind};
use crate::portal_runtime::{self, LOAD_TIMEOUT};
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use uuid::Uuid;
use webkit6::prelude::*;

/// A history step (`duetctl portal back|forward|reload`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortalStep {
    Back,
    Forward,
    Reload,
}

/// After a click or a submit, how long to give the page to start a
/// navigation before answering (if it did start one, the answer waits for
/// it to finish, so the caller's next `portal text` sees the new page).
const SETTLE_DELAY: Duration = Duration::from_millis(250);

/// A portal's persisted record and the edges of the workspace it lives in.
struct PortalTarget {
    payload: PortalPayload,
    edges: Vec<EdgeRecord>,
}

impl App {
    /// Finds portal `id` in the active workspace or any dormant one.
    fn portal_target(&self, id: Uuid) -> Result<PortalTarget, String> {
        if let Some(entry) = self.nodes.get(&id) {
            return entry
                .record
                .as_portal()
                .map(|payload| PortalTarget {
                    payload: payload.clone(),
                    edges: self.edges.clone(),
                })
                .ok_or_else(|| format!("{id} is not a portal"));
        }
        for workspace in &self.inactive_workspaces {
            if let Some(node) = workspace.nodes.iter().find(|node| node.id == id) {
                return node
                    .as_portal()
                    .map(|payload| PortalTarget {
                        payload: payload.clone(),
                        edges: workspace.edges.clone(),
                    })
                    .ok_or_else(|| format!("{id} is not a portal"));
            }
        }
        Err(format!("no portal with id {id}"))
    }

    /// Applies `edit` to portal `id`'s persisted payload, wherever it
    /// lives. Returns whether a portal was found.
    fn edit_portal(&mut self, id: Uuid, edit: impl FnOnce(&mut PortalPayload)) -> bool {
        if let Some(payload) = self
            .nodes
            .get_mut(&id)
            .and_then(|entry| entry.record.as_portal_mut())
        {
            edit(payload);
            return true;
        }
        for workspace in &mut self.inactive_workspaces {
            if let Some(payload) = workspace
                .nodes
                .iter_mut()
                .find(|node| node.id == id)
                .and_then(NodeRecord::as_portal_mut)
            {
                edit(payload);
                return true;
            }
        }
        false
    }

    /// Turns a `duetctl portal` argument into a portal id: a stable id, an
    /// `@portal:name` / `@name` reference, or a bare name. Inside a portal
    /// command an unqualified reference means a portal (the way a path
    /// means a file inside `duetctl file`), but resolution itself — name
    /// matching, caller-context narrowing, ambiguity — is the shared
    /// resolver's, so two portals with the same name are reported as
    /// ambiguous here exactly like any other duplicate resource.
    pub fn resolve_portal_argument(
        &self,
        raw: &str,
        requested_by: Option<Uuid>,
    ) -> Result<Uuid, String> {
        let raw = raw.trim();
        if let Ok(id) = Uuid::parse_str(raw) {
            self.portal_target(id)?;
            return Ok(id);
        }
        let reference = match raw.strip_prefix('@') {
            Some(body) if body.contains(':') => raw.to_string(),
            Some(body) => format!("@portal:{body}"),
            None => format!("@portal:{raw}"),
        };
        match self.resolve_resource(&reference, requested_by)? {
            ResolveOutcome::Found { resource } if resource.kind == ResourceKind::Portal => {
                Ok(resource.id)
            }
            ResolveOutcome::Found { resource } => Err(format!(
                "{raw} is the {} '{}', not a portal",
                resource.kind.label(),
                resource.name
            )),
            ResolveOutcome::Ambiguous { candidates } => Err(format!(
                "{raw} is ambiguous — it matches {} portals: {}. Use a portal id instead",
                candidates.len(),
                candidates
                    .iter()
                    .map(|c| format!("'{}' {} (workspace '{}')", c.name, c.id, c.workspace_name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            ResolveOutcome::NotFound => Err(format!("no portal named {raw}")),
        }
    }

    /// `duetctl portal list`: for an agent, every portal one edge away from
    /// it (discovery, like `notes list`), each marked with whether it may
    /// control it; for the human operator, every portal in the active
    /// workspace.
    pub fn list_portals(&self, requested_by: Option<Uuid>) -> Vec<PortalSummary> {
        let views = self.workspace_views();
        let mut summaries: Vec<PortalSummary> = match requested_by {
            None => views[0]
                .nodes
                .iter()
                .filter_map(|node| self.portal_summary_for(node.id, None))
                .collect(),
            Some(agent) => views
                .iter()
                .find(|view| view.nodes.iter().any(|node| node.id == agent))
                .map(|view| {
                    portals_connected_to(&view.nodes, &view.edges, agent)
                        .into_iter()
                        .filter_map(|id| self.portal_summary_for(id, Some(agent)))
                        .collect()
                })
                .unwrap_or_default(),
        };
        summaries.sort_by_key(|summary| summary.name.to_lowercase());
        summaries
    }

    /// One portal's summary as `requested_by` may see it.
    pub fn portal_summary_for(
        &self,
        id: Uuid,
        requested_by: Option<Uuid>,
    ) -> Option<PortalSummary> {
        let target = self.portal_target(id).ok()?;
        let controllable = authorize_portal(&target.edges, requested_by, id).is_ok();
        Some(PortalSummary {
            id,
            name: target.payload.name.clone(),
            url: controllable.then(|| self.current_portal_url(id, &target.payload)),
            controllable,
            allow_scripts: target.payload.allow_scripts,
        })
    }

    /// The URL portal `id` is on right now: the live view's, or the
    /// persisted one if it has no view yet.
    fn current_portal_url(&self, id: Uuid, payload: &PortalPayload) -> String {
        self.portals
            .view(id)
            .and_then(|view| view.uri())
            .map(|uri| uri.to_string())
            .unwrap_or_else(|| payload.url.clone())
    }

    /// `duetctl portal inspect` (and `url`/`title`): needs `ControlPortal`.
    pub fn inspect_portal(
        &self,
        requested_by: Option<Uuid>,
        id: Uuid,
    ) -> Result<PortalInfo, String> {
        let target = self.portal_target(id)?;
        authorize_portal(&target.edges, requested_by, id)?;
        let view = self.portals.view(id);
        let controllers = {
            let views = self.workspace_views();
            let view_of_portal = views
                .iter()
                .find(|view| view.nodes.iter().any(|node| node.id == id));
            view_of_portal
                .map(|workspace| {
                    workspace
                        .nodes
                        .iter()
                        .filter_map(|node| {
                            let terminal = node.as_terminal()?;
                            authorize_portal(&workspace.edges, Some(node.id), id)
                                .is_ok()
                                .then(|| terminal.name.clone())
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(PortalInfo {
            id,
            name: target.payload.name.clone(),
            url: self.current_portal_url(id, &target.payload),
            title: view
                .as_ref()
                .and_then(|view| view.title())
                .map(|title| title.to_string())
                .filter(|title| !title.is_empty()),
            loading: view.as_ref().is_some_and(|view| view.is_loading()),
            can_go_back: view.as_ref().is_some_and(|view| view.can_go_back()),
            can_go_forward: view.as_ref().is_some_and(|view| view.can_go_forward()),
            profile_id: target.payload.profile.id,
            storage: match target.payload.profile.storage {
                PortalStorage::Persistent => "persistent".to_string(),
                PortalStorage::Ephemeral => "ephemeral".to_string(),
            },
            allow_scripts: target.payload.allow_scripts,
            controllers,
        })
    }

    /// Portal `id`'s live view, created (loading its persisted URL) if this
    /// run hasn't shown it yet — e.g. a portal in a workspace that hasn't
    /// been opened since Duet started.
    pub fn ensure_portal_view(
        app: &Rc<RefCell<App>>,
        id: Uuid,
    ) -> Result<webkit6::WebView, String> {
        let payload = app.borrow().portal_target(id)?.payload;
        let (view, created) = app
            .borrow_mut()
            .portals
            .ensure(id, &payload.profile, &payload.url);
        if created {
            wire_view_sync(app, id, &view);
        }
        Ok(view)
    }

    /// Mirrors a portal view's live state into its persisted record (the
    /// URL, so a restart reopens the same page) and onto its card.
    fn sync_portal_state(app: &Rc<RefCell<App>>, id: Uuid) {
        let Some(view) = app.borrow().portals.view(id) else {
            return;
        };
        let uri = view.uri().map(|uri| uri.to_string()).unwrap_or_default();
        let title = view
            .title()
            .map(|title| title.to_string())
            .unwrap_or_default();
        // Only URLs a portal is allowed to load are persisted — an
        // error page or a `data:` URL from a redirect isn't restorable.
        let persistable = normalize_url(&uri).is_ok_and(|normalized| normalized == uri);
        let changed = persistable && {
            let mut app_mut = app.borrow_mut();
            let mut changed = false;
            app_mut.edit_portal(id, |payload| {
                if payload.url != uri {
                    payload.url = uri.clone();
                    changed = true;
                }
            });
            changed
        };
        if changed {
            App::schedule_persist(app);
        }
        let node = match app.borrow().nodes.get(&id).map(|entry| &entry.widget) {
            Some(NodeWidget::Portal(node)) => Some(node.clone()),
            _ => None,
        };
        if let Some(node) = node {
            node.show_url(&uri);
            node.show_page_title(&title);
            node.show_navigation_state(
                view.can_go_back(),
                view.can_go_forward(),
                view.is_loading(),
            );
        }
    }

    /// Authorizes `requested_by` on portal `id` (arbitrary scripts too,
    /// when `script`) and returns its live view.
    fn authorized_portal_view(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        script: bool,
    ) -> Result<webkit6::WebView, String> {
        let target = app.borrow().portal_target(id)?;
        if script {
            authorize_script(
                &target.edges,
                requested_by,
                id,
                target.payload.allow_scripts,
            )?;
        } else {
            authorize_portal(&target.edges, requested_by, id)?;
        }
        App::ensure_portal_view(app, id)
    }

    /// `duetctl portal navigate` and the card's URL field: loads `url`
    /// (validated by `orchestration::portal::normalize_url`) and answers
    /// once the page has loaded.
    pub fn portal_navigate(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        url: &str,
        done: impl FnOnce(Result<PortalAction, String>) + 'static,
    ) {
        let url = match normalize_url(url) {
            Ok(url) => url,
            Err(error) => return done(Err(error)),
        };
        let view = match App::authorized_portal_view(app, requested_by, id, false) {
            Ok(view) => view,
            Err(error) => return done(Err(error)),
        };
        let detail = format!("navigated to {url}");
        portal_runtime::load_and_wait(
            &view.clone(),
            LOAD_TIMEOUT,
            move |view| view.load_uri(&url),
            move |result| match result {
                Ok(()) => report_action(id, &view, detail, done),
                Err(error) => done(Err(error)),
            },
        );
    }

    /// `duetctl portal back|forward|reload` and the card's buttons.
    pub fn portal_step(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        step: PortalStep,
        done: impl FnOnce(Result<PortalAction, String>) + 'static,
    ) {
        let view = match App::authorized_portal_view(app, requested_by, id, false) {
            Ok(view) => view,
            Err(error) => return done(Err(error)),
        };
        let (allowed, detail) = match step {
            PortalStep::Back => (view.can_go_back(), "went back"),
            PortalStep::Forward => (view.can_go_forward(), "went forward"),
            PortalStep::Reload => (true, "reloaded"),
        };
        if !allowed {
            return done(Err(format!(
                "nothing to go {} to in this portal's history",
                if step == PortalStep::Back {
                    "back"
                } else {
                    "forward"
                }
            )));
        }
        portal_runtime::load_and_wait(
            &view.clone(),
            LOAD_TIMEOUT,
            move |view| match step {
                PortalStep::Back => view.go_back(),
                PortalStep::Forward => view.go_forward(),
                // Bypassing the cache: the point of reloading after an edit
                // is to see the edit, not a cached copy of the old page.
                PortalStep::Reload => view.reload_bypass_cache(),
            },
            move |result| match result {
                Ok(()) => report_action(id, &view, detail.to_string(), done),
                Err(error) => done(Err(error)),
            },
        );
    }

    /// `duetctl portal text`: the page's URL, title and readable text — or
    /// the outer HTML (`html`) — of the whole page or of the first element
    /// matching `selector`, at most `limit` characters.
    pub fn portal_text(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        selector: Option<String>,
        html: bool,
        limit: Option<usize>,
        done: impl FnOnce(Result<PortalPage, String>) + 'static,
    ) {
        let view = match App::authorized_portal_view(app, requested_by, id, false) {
            Ok(view) => view,
            Err(error) => return done(Err(error)),
        };
        let script = domain::text_script(selector.as_deref(), html);
        when_loaded(&view, move |view| {
            portal_runtime::run_script(&view, &script, true, move |result| {
                done(
                    result
                        .and_then(|raw| parse_script_reply(&raw))
                        .map(|reply| {
                            let (text, truncated) = domain::truncate_text(
                                reply.text.as_deref().unwrap_or(""),
                                limit.unwrap_or(DEFAULT_TEXT_LIMIT),
                            );
                            PortalPage {
                                id,
                                url: reply.url.unwrap_or_default(),
                                title: reply.title.unwrap_or_default(),
                                text,
                                truncated,
                            }
                        }),
                )
            });
        });
    }

    /// `duetctl portal screenshot`: captures the page as a PNG under Duet's
    /// own data directory (`orchestration::portal::screenshot_path` — never
    /// a caller-chosen path), keeping the newest few per portal.
    pub fn portal_screenshot(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        full_page: bool,
        done: impl FnOnce(Result<PortalScreenshot, String>) + 'static,
    ) {
        let view = match App::authorized_portal_view(app, requested_by, id, false) {
            Ok(view) => view,
            Err(error) => return done(Err(error)),
        };
        let base = app.borrow().portals.base_dir().to_path_buf();
        when_loaded(&view, move |view| {
            let url = view.uri().map(|uri| uri.to_string()).unwrap_or_default();
            portal_runtime::capture(&view, full_page, move |result| {
                done(result.and_then(|texture| {
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_millis())
                        .unwrap_or_default();
                    let path = domain::screenshot_path(&base, id, stamp);
                    let dir = path.parent().expect("screenshot paths have a directory");
                    std::fs::create_dir_all(dir)
                        .map_err(|error| format!("couldn't create {}: {error}", dir.display()))?;
                    // A screenshot can show anything a logged-in page does.
                    portal_runtime::restrict_to_owner(dir);
                    texture
                        .save_to_png(&path)
                        .map_err(|error| format!("couldn't save the screenshot: {error}"))?;
                    domain::prune_screenshots(dir, SCREENSHOTS_KEPT_PER_PORTAL);
                    Ok(PortalScreenshot {
                        id,
                        path: path.display().to_string(),
                        width: texture.width(),
                        height: texture.height(),
                        url,
                    })
                }))
            });
        });
    }

    /// `duetctl portal click`: clicks the first element matching `selector`.
    pub fn portal_click(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        selector: &str,
        done: impl FnOnce(Result<PortalAction, String>) + 'static,
    ) {
        let script = domain::click_script(selector);
        App::portal_interact(app, requested_by, id, script, done);
    }

    /// `duetctl portal type`: types `text` into the first element matching
    /// `selector` (replacing its value unless `append`), optionally
    /// submitting its form.
    #[allow(clippy::too_many_arguments)]
    pub fn portal_type(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        selector: &str,
        text: &str,
        append: bool,
        submit: bool,
        done: impl FnOnce(Result<PortalAction, String>) + 'static,
    ) {
        let script = domain::type_script(selector, text, append, submit);
        App::portal_interact(app, requested_by, id, script, done);
    }

    /// Runs one of Duet's own interaction scripts (in its isolated world),
    /// then lets the page settle: if the interaction started a navigation,
    /// the answer waits for it.
    fn portal_interact(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        script: String,
        done: impl FnOnce(Result<PortalAction, String>) + 'static,
    ) {
        let view = match App::authorized_portal_view(app, requested_by, id, false) {
            Ok(view) => view,
            Err(error) => return done(Err(error)),
        };
        when_loaded(&view, move |view| {
            portal_runtime::run_script(&view.clone(), &script, true, move |result| {
                match result.and_then(|raw| parse_script_reply(&raw)) {
                    Err(error) => done(Err(error)),
                    Ok(reply) => {
                        let detail = reply.detail.unwrap_or_else(|| "done".to_string());
                        glib::timeout_add_local_once(SETTLE_DELAY, move || {
                            when_loaded(&view, move |view| report_action(id, &view, detail, done));
                        });
                    }
                }
            });
        });
    }

    /// `duetctl portal evaluate`: runs arbitrary JavaScript in the page's
    /// own world and returns its (JSON) result. A privileged operation: an
    /// agent needs `ControlPortal` *and* the portal's `allow_scripts`.
    pub fn portal_evaluate(
        app: &Rc<RefCell<App>>,
        requested_by: Option<Uuid>,
        id: Uuid,
        script: &str,
        done: impl FnOnce(Result<serde_json::Value, String>) + 'static,
    ) {
        let view = match App::authorized_portal_view(app, requested_by, id, true) {
            Ok(view) => view,
            Err(error) => return done(Err(error)),
        };
        let script = script.to_string();
        when_loaded(&view, move |view| {
            let expression = domain::evaluate_script(&script, EvaluateForm::Expression);
            portal_runtime::run_script(&view.clone(), &expression, false, move |result| {
                match result {
                    // Not an expression; nothing ran — try it as statements.
                    Err(error) if domain::is_parse_failure(&error) => {
                        let statements = domain::evaluate_script(&script, EvaluateForm::Statements);
                        portal_runtime::run_script(&view, &statements, false, move |result| {
                            done(evaluated(result))
                        });
                    }
                    result => done(evaluated(result)),
                }
            });
        });
    }

    /// Creates a new portal (with its own isolated profile) at `position`,
    /// named `name` or, if `None`, the first free "Portal N".
    pub fn create_portal(
        app: &Rc<RefCell<App>>,
        name: Option<&str>,
        url: &str,
        position: (f64, f64),
    ) -> Result<Uuid, String> {
        let name = match name {
            Some(name) => domain::validate_portal_name(name)?,
            None => app.borrow().unused_portal_name(),
        };
        let url = if url.trim().is_empty() {
            String::new()
        } else {
            normalize_url(url)?
        };
        let record = NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position,
            size: (640.0, 480.0),
            z_order: next_z_order(&app.borrow()),
            collapsed: false,
            locked: false,
            kind: NodeKind::Portal(PortalPayload::new(&name, &url)),
        };
        let id = record.id;
        materialize_node(app, record.clone(), &App::toaster(app))?;
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

    /// "Portal", then "Portal 2", "Portal 3"... — whichever no portal in
    /// the active workspace is called yet.
    fn unused_portal_name(&self) -> String {
        let taken: Vec<String> = self
            .nodes
            .values()
            .filter_map(|entry| entry.record.as_portal())
            .map(|portal| portal.name.to_lowercase())
            .collect();
        (1..)
            .map(|n| match n {
                1 => crate::model::DEFAULT_PORTAL_NAME.to_string(),
                n => format!("{} {n}", crate::model::DEFAULT_PORTAL_NAME),
            })
            .find(|name| !taken.contains(&name.to_lowercase()))
            .expect("an unbounded range always has a free name")
    }

    /// Renames a portal (its alias for `@portal:<name>`; its id, and so
    /// every connection and agent reference by id, is unchanged).
    pub fn rename_portal(app: &Rc<RefCell<App>>, id: Uuid, name: &str) -> Result<(), String> {
        let name = domain::validate_portal_name(name)?;
        if !app
            .borrow_mut()
            .edit_portal(id, |payload| payload.name = name.clone())
        {
            return Err(format!("no portal with id {id}"));
        }
        if let Some(entry) = app.borrow().nodes.get(&id)
            && let NodeWidget::Portal(node) = &entry.widget
        {
            node.set_name(&name);
        }
        let _ = app.borrow().persist();
        Ok(())
    }

    /// The per-portal opt-in for agents' arbitrary script evaluation.
    pub fn set_portal_allow_scripts(app: &Rc<RefCell<App>>, id: Uuid, allow: bool) {
        app.borrow_mut()
            .edit_portal(id, |payload| payload.allow_scripts = allow);
        let _ = app.borrow().persist();
    }

    /// "Open externally": hands the portal's current page to the desktop's
    /// default browser. Human-only — nothing agent-facing calls this.
    pub fn open_portal_externally(app: &Rc<RefCell<App>>, id: Uuid) -> Result<(), String> {
        let url = {
            let app_ref = app.borrow();
            let target = app_ref.portal_target(id)?;
            app_ref.current_portal_url(id, &target.payload)
        };
        let url = normalize_url(&url).map_err(|_| "this portal has no page to open".to_string())?;
        if url == "about:blank" {
            return Err("this portal has no page to open".to_string());
        }
        gtk4::gio::AppInfo::launch_default_for_uri(&url, None::<&gtk4::gio::AppLaunchContext>)
            .map_err(|error| format!("couldn't open {url}: {error}"))
    }

    /// Clears the cookies, storage and cache of portal `id`'s profile.
    pub fn clear_portal_data(app: &Rc<RefCell<App>>, id: Uuid) {
        let Ok(target) = app.borrow().portal_target(id) else {
            return;
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        app.borrow_mut()
            .portals
            .clear_profile_data(&target.payload.profile, move |result| {
                let _ = sender.send(result);
            });
        let app = Rc::downgrade(app);
        glib::timeout_add_local(Duration::from_millis(100), move || {
            match receiver.try_recv() {
                Ok(result) => {
                    if let Some(app) = app.upgrade() {
                        App::notify(
                            &app,
                            &match result {
                                Ok(()) => "Browsing data cleared".to_string(),
                                Err(error) => format!("Couldn't clear browsing data: {error}"),
                            },
                        );
                    }
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
            }
        });
    }

    /// Every dev-server URL `terminal` has printed, newest first.
    pub fn detected_dev_urls(&self, terminal: Uuid) -> Vec<String> {
        self.dev_urls
            .get(&terminal)
            .map(|scanner| scanner.detected.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    /// Offers each newly detected dev-server URL as a toast with an "Open
    /// in Portal" button. Never navigates anything by itself: only the
    /// user's click does.
    pub fn offer_dev_server_urls(app: &Rc<RefCell<App>>) {
        let pending: Vec<(Uuid, String)> = std::mem::take(&mut app.borrow_mut().pending_dev_urls);
        for (terminal, url) in pending {
            let name = super::node_display_name(&app.borrow(), terminal);
            let toast = adw::Toast::new(&format!("{name} is serving {url}"));
            toast.set_button_label(Some("Open in Portal"));
            toast.set_timeout(10);
            toast.connect_button_clicked({
                let app = Rc::clone(app);
                move |_| {
                    if let Err(error) = App::open_url_in_portal(&app, Some(terminal), &url) {
                        App::notify(&app, &error);
                    }
                }
            });
            App::toaster(app).add_toast(toast);
        }
    }

    /// "Open in Portal" (a user action, from the dev-server toast or a
    /// terminal's menu): navigates the portal `terminal` controls, or — if
    /// it controls none — creates one beside it, connected with
    /// `ControlPortal` so the agent can drive it too.
    pub fn open_url_in_portal(
        app: &Rc<RefCell<App>>,
        terminal: Option<Uuid>,
        url: &str,
    ) -> Result<Uuid, String> {
        let url = normalize_url(url)?;
        let existing = terminal.and_then(|terminal| {
            let app_ref = app.borrow();
            let mut portals: Vec<(String, Uuid)> = app_ref
                .nodes
                .values()
                .filter_map(|entry| {
                    let portal = entry.record.as_portal()?;
                    authorize_portal(&app_ref.edges, Some(terminal), entry.record.id)
                        .is_ok()
                        .then(|| (portal.name.to_lowercase(), entry.record.id))
                })
                .collect();
            portals.sort();
            portals.first().map(|(_, id)| *id)
        });
        let id = match existing {
            Some(id) => id,
            None => {
                let (position, name) = {
                    let app_ref = app.borrow();
                    match terminal.and_then(|id| app_ref.nodes.get(&id)) {
                        Some(entry) => (
                            (
                                entry.record.position.0 + entry.record.size.0 + 40.0,
                                entry.record.position.1,
                            ),
                            entry
                                .record
                                .as_terminal()
                                .map(|t| format!("{} preview", t.name))
                                .filter(|name| domain::validate_portal_name(name).is_ok()),
                        ),
                        None => ((160.0, 160.0), None),
                    }
                };
                let id = App::create_portal(app, name.as_deref(), "", position)?;
                if let Some(terminal) = terminal {
                    App::create_edge(app, terminal, id);
                }
                id
            }
        };
        App::portal_navigate(app, None, id, &url, {
            let app = Rc::downgrade(app);
            move |result| {
                if let (Err(error), Some(app)) = (result, app.upgrade()) {
                    App::notify(&app, &error);
                }
            }
        });
        Ok(id)
    }
}

/// Keeps portal `id`'s record and card in step with its view: every URL,
/// title or loading-state change schedules one `sync_portal_state`. Wired
/// exactly once per view, when the runtime creates it.
fn wire_view_sync(app: &Rc<RefCell<App>>, id: Uuid, view: &webkit6::WebView) {
    let sync = {
        let app = Rc::downgrade(app);
        move |_: &webkit6::WebView| {
            let app = app.clone();
            glib::idle_add_local_once(move || {
                if let Some(app) = app.upgrade() {
                    App::sync_portal_state(&app, id);
                }
            });
        }
    };
    view.connect_uri_notify(sync.clone());
    view.connect_title_notify(sync.clone());
    view.connect_is_loading_notify(sync);
}

/// An `evaluate` call's raw result as its JSON value.
fn evaluated(result: Result<String, String>) -> Result<serde_json::Value, String> {
    result
        .and_then(|raw| parse_script_reply(&raw))
        .map(|reply| reply.value.unwrap_or(serde_json::Value::Null))
}

/// Answers a navigation/interaction with where the page is now. Read from
/// the page itself (`location.href`, `document.title`, in Duet's isolated
/// world) rather than the view's `uri`/`title` properties, which WebKit
/// updates a moment *after* a load reports finished — so the answer never
/// races the page. Falls back to those properties if the page can't run
/// the script (an error page, say).
fn report_action(
    id: Uuid,
    view: &webkit6::WebView,
    detail: String,
    done: impl FnOnce(Result<PortalAction, String>) + 'static,
) {
    let fallback = PortalAction {
        id,
        url: view.uri().map(|uri| uri.to_string()).unwrap_or_default(),
        title: view
            .title()
            .map(|title| title.to_string())
            .filter(|title| !title.is_empty()),
        detail,
    };
    portal_runtime::run_script(view, &domain::location_script(), true, move |result| {
        done(Ok(match result.and_then(|raw| parse_script_reply(&raw)) {
            Ok(reply) => PortalAction {
                url: reply.url.unwrap_or(fallback.url),
                title: reply.title.filter(|title| !title.is_empty()),
                ..fallback
            },
            Err(_) => fallback,
        }))
    });
}

/// Runs `then` once `view` isn't loading (immediately if it isn't), so a
/// read right after a navigation sees the new page, not the old one.
/// A load that times out or fails still runs `then`: reading whatever the
/// page shows (an error page, a half-loaded app) is more useful to the
/// caller than a refusal.
fn when_loaded(view: &webkit6::WebView, then: impl FnOnce(webkit6::WebView) + 'static) {
    if !view.is_loading() {
        return then(view.clone());
    }
    let waited = view.clone();
    portal_runtime::load_and_wait(view, LOAD_TIMEOUT, |_| {}, move |_| then(waited));
}

/// Builds a Portal card for portal `id` and shows its live view in it.
pub(super) fn materialize_portal(
    app: &Rc<RefCell<App>>,
    id: Uuid,
    payload: &PortalPayload,
    collapsed: bool,
    toast_overlay: &adw::ToastOverlay,
) -> Result<NodeWidget, String> {
    let node = PortalNode::new(&payload.name, &payload.url, collapsed, {
        let app = Rc::clone(app);
        move |collapsed| {
            if let Some(entry) = app.borrow_mut().nodes.get_mut(&id) {
                entry.record.collapsed = collapsed;
            }
            App::schedule_persist(&app);
        }
    });
    // The record isn't in `app.nodes` yet (`materialize_node` inserts it
    // after this returns), so the view is created from `payload` directly.
    let (view, created) = app
        .borrow_mut()
        .portals
        .ensure(id, &payload.profile, &payload.url);
    if created {
        wire_view_sync(app, id, &view);
    }
    node.attach_view(&view);
    node.show_url(
        &view
            .uri()
            .map(|uri| uri.to_string())
            .unwrap_or_else(|| payload.url.clone()),
    );
    node.show_navigation_state(view.can_go_back(), view.can_go_forward(), view.is_loading());

    let report = {
        let toast_overlay = toast_overlay.clone();
        move |result: Result<PortalAction, String>| {
            if let Err(error) = result {
                toast_overlay.add_toast(adw::Toast::new(&error));
            }
        }
    };
    node.url_entry.connect_activate({
        let app = Rc::clone(app);
        let report = report.clone();
        move |entry| {
            let url = entry.text().to_string();
            // Give the focus back to the page so the field shows the real
            // URL (redirects included) as the page loads.
            if let Some(view) = app.borrow().portals.view(id) {
                view.grab_focus();
            }
            App::portal_navigate(&app, None, id, &url, report.clone());
        }
    });
    for (button, step) in [
        (&node.back_button, PortalStep::Back),
        (&node.forward_button, PortalStep::Forward),
        (&node.reload_button, PortalStep::Reload),
    ] {
        button.connect_clicked({
            let app = Rc::clone(app);
            let report = report.clone();
            move |_| App::portal_step(&app, None, id, step, report.clone())
        });
    }
    node.external_button.connect_clicked({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        move |_| {
            if let Err(error) = App::open_portal_externally(&app, id) {
                toast_overlay.add_toast(adw::Toast::new(&error));
            }
        }
    });
    wire_portal_rename(app, &node, id, toast_overlay);
    Ok(NodeWidget::Portal(node))
}

/// Double-click the title to rename; Enter commits, Escape cancels.
fn wire_portal_rename(
    app: &Rc<RefCell<App>>,
    node: &PortalNode,
    id: Uuid,
    toast_overlay: &adw::ToastOverlay,
) {
    let click = gtk4::GestureClick::new();
    click.connect_pressed({
        let label = node.title_label.clone();
        let entry = node.title_entry.clone();
        move |_, presses, _, _| {
            if presses == 2 {
                crate::node::set_renaming(&label, &entry, true);
            }
        }
    });
    node.title_label.add_controller(click);
    let finish = {
        let label = node.title_label.clone();
        let entry = node.title_entry.clone();
        move || crate::node::set_renaming(&label, &entry, false)
    };
    node.title_entry.connect_activate({
        let app = Rc::clone(app);
        let toast_overlay = toast_overlay.clone();
        let finish = finish.clone();
        move |entry| match App::rename_portal(&app, id, &entry.text()) {
            Ok(()) => finish(),
            Err(error) => toast_overlay.add_toast(adw::Toast::new(&error)),
        }
    });
    let keys = gtk4::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk4::gdk::Key::Escape {
            finish();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    node.title_entry.add_controller(keys);
}

/// A Portal card's own menu entries.
pub(super) fn portal_menu_items(app: &Rc<RefCell<App>>, id: Uuid) -> Vec<MenuItem> {
    let Some(payload) = app
        .borrow()
        .nodes
        .get(&id)
        .and_then(|entry| entry.record.as_portal().cloned())
    else {
        return Vec::new();
    };
    let mut items: Vec<MenuItem> = Vec::new();
    let app_c = Rc::clone(app);
    items.push((
        "Rename…\tdouble-click".to_string(),
        Box::new(move || {
            if let Some(entry) = app_c.borrow().nodes.get(&id)
                && let NodeWidget::Portal(node) = &entry.widget
            {
                crate::node::set_renaming(&node.title_label, &node.title_entry, true);
            }
        }),
    ));
    let app_c = Rc::clone(app);
    items.push((
        "Open in default browser".to_string(),
        Box::new(move || {
            if let Err(error) = App::open_portal_externally(&app_c, id) {
                App::notify(&app_c, &error);
            }
        }),
    ));
    let app_c = Rc::clone(app);
    let reference = format!("@portal:{}", payload.name);
    items.push((
        format!("Copy reference ({reference})"),
        Box::new(move || {
            if let Some(display) = gtk4::gdk::Display::default() {
                display.clipboard().set_text(&reference);
                App::notify(&app_c, &format!("Copied {reference}"));
            }
        }),
    ));
    items.push(separator());
    let app_c = Rc::clone(app);
    let allow = payload.allow_scripts;
    items.push((
        check_label("Allow agents to run JavaScript", allow),
        Box::new(move || App::set_portal_allow_scripts(&app_c, id, !allow)),
    ));
    let app_c = Rc::clone(app);
    items.push((
        "Clear browsing data".to_string(),
        Box::new(move || App::clear_portal_data(&app_c, id)),
    ));
    items
}

/// A Terminal card's "Open in Portal" entries, one per dev-server URL it
/// has printed (newest first, at most five).
pub(super) fn terminal_portal_menu_items(app: &Rc<RefCell<App>>, id: Uuid) -> Vec<MenuItem> {
    app.borrow()
        .detected_dev_urls(id)
        .into_iter()
        .take(5)
        .map(|url| {
            let app_c = Rc::clone(app);
            let label = format!("Open {url} in Portal");
            let item: MenuItem = (
                label,
                Box::new(move || {
                    if let Err(error) = App::open_url_in_portal(&app_c, Some(id), &url) {
                        App::notify(&app_c, &error);
                    }
                }),
            );
            item
        })
        .collect()
}

/// Forgets the runtime state of a node that's leaving the canvas for good
/// (deleted, not switched away from): a portal's view is dropped; its
/// profile's stored data stays, so undo brings it back logged in.
pub(super) fn forget_node(app: &mut App, id: Uuid) {
    app.portals.remove(id);
    app.dev_urls.remove(&id);
}

/// Gives a duplicated or pasted portal its own fresh profile (same storage
/// mode): isolation by default means a copy never silently shares the
/// original's cookies.
pub(super) fn isolate_duplicate(payload: &mut PortalPayload) {
    payload.profile.id = Uuid::new_v4();
}

/// Whether `capability` should be granted by default on a new edge between
/// these two node kinds: a Terminal and a Portal get `ControlPortal`.
pub(super) fn default_portal_capability(a: &NodeKind, b: &NodeKind) -> Option<EdgeCapability> {
    matches!(
        (a, b),
        (NodeKind::Terminal(_), NodeKind::Portal(_)) | (NodeKind::Portal(_), NodeKind::Terminal(_))
    )
    .then_some(EdgeCapability::ControlPortal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountStore;
    use crate::agent::Agent;
    use crate::model::{EnvironmentKind, TerminalPayload};
    use std::io::{Read as _, Write as _};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    /// A deterministic local test website (never a public one): serves
    /// files from `root`, plus `/set-cookie?<value>` (sets `session=<value>`)
    /// and `/cookie` (echoes the request's `Cookie` header) for profile
    /// isolation checks. Returns its base URL.
    fn serve(root: PathBuf) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let root = root.clone();
                std::thread::spawn(move || answer(stream, &root));
            }
        });
        base
    }

    fn answer(mut stream: std::net::TcpStream, root: &Path) {
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        let request = String::from_utf8_lossy(&request).to_string();
        let target = request.split_whitespace().nth(1).unwrap_or("/").to_string();
        let cookie = request
            .lines()
            .find_map(|line| {
                line.strip_prefix("Cookie: ")
                    .or(line.strip_prefix("cookie: "))
            })
            .unwrap_or("none")
            .to_string();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let mut headers = String::new();
        let (status, content_type, body) = match path {
            "/set-cookie" => {
                headers = format!("Set-Cookie: session={query}; Path=/; Max-Age=3600\r\n");
                (
                    "200 OK",
                    "text/html",
                    b"<title>cookie set</title><p>set</p>".to_vec(),
                )
            }
            "/csp" => {
                headers = "Content-Security-Policy: default-src 'none'; script-src 'none'\r\n"
                    .to_string();
                (
                    "200 OK",
                    "text/html",
                    b"<title>Strict</title><h1 id=\"section\">No scripts here</h1>".to_vec(),
                )
            }
            "/cookie" => (
                "200 OK",
                "text/html",
                format!("<title>cookie</title><p id=\"cookie\">{cookie}</p>").into_bytes(),
            ),
            _ => {
                let relative = path.trim_start_matches('/');
                let relative = if relative.is_empty() {
                    "index.html"
                } else {
                    relative
                };
                match std::fs::read(root.join(relative)) {
                    Ok(body) => (
                        "200 OK",
                        if relative.ends_with(".css") {
                            "text/css"
                        } else {
                            "text/html"
                        },
                        body,
                    ),
                    Err(_) => ("404 Not Found", "text/html", b"<title>404</title>".to_vec()),
                }
            }
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{headers}Connection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
    }

    /// Runs `start` (an async `PortalService` call taking a `done`
    /// callback) and spins the GLib main loop until `done` fires.
    fn wait<T: 'static>(start: impl FnOnce(Box<dyn FnOnce(T)>)) -> T {
        let slot: Rc<RefCell<Option<T>>> = Rc::new(RefCell::new(None));
        let sink = slot.clone();
        start(Box::new(move |value| *sink.borrow_mut() = Some(value)));
        spin_until(|| slot.borrow().is_some());
        slot.take().unwrap()
    }

    fn spin_until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let context = glib::MainContext::default();
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for the page");
            if !context.iteration(false) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn test_app(root: &Path) -> Rc<RefCell<App>> {
        let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
        let app = App::new(
            AccountStore::new(tmp.join("accounts")),
            tmp.join("store.json"),
        );
        app.borrow_mut().workspace_root = root.to_path_buf();
        app
    }

    fn add_shell_agent(app: &Rc<RefCell<App>>, name: &str, cwd: &Path) -> Uuid {
        let record = NodeRecord {
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
        };
        let id = record.id;
        materialize_node(app, record, &adw::ToastOverlay::new()).unwrap();
        id
    }

    fn navigate(
        app: &Rc<RefCell<App>>,
        by: Option<Uuid>,
        id: Uuid,
        url: &str,
    ) -> Result<PortalAction, String> {
        wait(|done| App::portal_navigate(app, by, id, url, done))
    }

    fn text(
        app: &Rc<RefCell<App>>,
        by: Option<Uuid>,
        id: Uuid,
        selector: Option<&str>,
    ) -> Result<PortalPage, String> {
        wait(|done| App::portal_text(app, by, id, selector.map(str::to_string), false, None, done))
    }

    const LOGIN_PAGE: &str = r#"<!doctype html><html><head><title>Login</title></head><body>
<h1>Welcome back</h1>
<form id="login" onsubmit="event.preventDefault(); document.getElementById('status').textContent = 'Signed in as ' + document.getElementById('user').value;">
<input id="user" name="user"><button id="go" type="submit">Sign in</button>
</form>
<p id="status">Not signed in</p>
<a id="next" href="/about.html">About</a>
</body></html>"#;

    /// Every PortalService operation against real WebKit views and a
    /// deterministic local site: resolution (qualified, unqualified,
    /// ambiguous), `ControlPortal` enforcement, navigation, URL/title/text,
    /// type/click/back, screenshots, privileged evaluation, profile
    /// isolation, and "Open in Portal". Needs a display:
    /// `cargo test portal_service_end_to_end -- --ignored --exact`.
    #[test]
    #[ignore = "needs a display"]
    fn portal_service_end_to_end_against_a_local_test_site() {
        if gtk4::init().is_err() {
            return;
        }
        let site = tempfile::tempdir().unwrap();
        std::fs::write(site.path().join("index.html"), LOGIN_PAGE).unwrap();
        std::fs::write(
            site.path().join("about.html"),
            "<title>About</title><h1>About page</h1>",
        )
        .unwrap();
        let base = serve(site.path().to_path_buf());
        let app = test_app(site.path());

        let frontend_agent = add_shell_agent(&app, "Frontend", site.path());
        let reviewer = add_shell_agent(&app, "Reviewer", site.path());
        let frontend = App::create_portal(&app, Some("Frontend"), "", (600.0, 0.0)).unwrap();
        let docs_a = App::create_portal(&app, Some("Docs"), "", (600.0, 500.0)).unwrap();
        let docs_b = App::create_portal(&app, Some("Docs"), "", (1300.0, 500.0)).unwrap();
        assert!(App::create_edge(&app, frontend_agent, frontend));
        // The connect gesture grants ControlPortal between agent and portal.
        let edge = app.borrow().edges.last().cloned().unwrap();
        assert!(edge.capabilities.contains(&EdgeCapability::ControlPortal));
        let me = Some(frontend_agent);

        // Resolution: qualified, unqualified (inside a portal command), by
        // id, and ambiguity reported exactly like other kinds.
        let resolve = |raw: &str| app.borrow().resolve_portal_argument(raw, me);
        assert_eq!(resolve("@portal:frontend"), Ok(frontend));
        assert_eq!(resolve("@frontend"), Ok(frontend));
        assert_eq!(resolve("frontend"), Ok(frontend));
        assert_eq!(resolve(&frontend.to_string()), Ok(frontend));
        assert!(resolve("@docs").unwrap_err().contains("ambiguous"));
        assert_eq!(resolve(&docs_a.to_string()), Ok(docs_a));
        assert!(
            resolve("@agent:frontend")
                .unwrap_err()
                .contains("not a portal")
        );
        assert!(resolve("@portal:nowhere").is_err());
        assert!(matches!(
            app.borrow().resolve_resource("@frontend", me).unwrap(),
            ResolveOutcome::Ambiguous { .. }
        ));

        // ControlPortal: an unconnected agent in the same workspace is
        // refused; the connected one and the human operator are not.
        let refused = navigate(&app, Some(reviewer), frontend, &base).unwrap_err();
        assert!(refused.contains("ControlPortal"), "{refused}");
        assert!(
            text(&app, me, docs_a, None)
                .unwrap_err()
                .contains("ControlPortal")
        );
        assert!(
            app.borrow()
                .inspect_portal(Some(reviewer), frontend)
                .is_err()
        );
        let listed = app.borrow().list_portals(me);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].controllable && listed[0].id == frontend);
        assert_eq!(app.borrow().list_portals(Some(reviewer)), Vec::new());
        assert_eq!(app.borrow().list_portals(None).len(), 3);

        // Navigation, URL, title and text.
        assert!(navigate(&app, me, frontend, "file:///etc/passwd").is_err());
        let action = navigate(&app, me, frontend, &format!("{base}/")).unwrap();
        assert_eq!(action.url, format!("{base}/"));
        assert_eq!(action.title.as_deref(), Some("Login"));
        // `inspect` reports the view's own properties, which WebKit updates
        // a moment after the load finishes.
        spin_until(|| {
            app.borrow()
                .inspect_portal(me, frontend)
                .unwrap()
                .title
                .as_deref()
                == Some("Login")
        });
        let info = app.borrow().inspect_portal(me, frontend).unwrap();
        assert_eq!(info.url, format!("{base}/"));
        assert_eq!(info.controllers, vec!["Frontend".to_string()]);
        let page = text(&app, me, frontend, None).unwrap();
        assert!(page.text.contains("Welcome back"), "{}", page.text);
        assert_eq!(page.title, "Login");
        assert_eq!(
            text(&app, me, frontend, Some("#status")).unwrap().text,
            "Not signed in"
        );
        let html =
            wait(|done| App::portal_text(&app, me, frontend, Some("h1".into()), true, None, done))
                .unwrap();
        assert_eq!(html.text, "<h1>Welcome back</h1>");
        assert!(text(&app, me, frontend, Some("#missing")).is_err());
        // The URL is persisted on the record (restored after a restart).
        spin_until(|| {
            app.borrow().nodes[&frontend]
                .record
                .as_portal()
                .unwrap()
                .url
                == format!("{base}/")
        });

        // Type (with a submit), then click a link that navigates.
        let typed =
            wait(|done| App::portal_type(&app, me, frontend, "#user", "ada", false, true, done))
                .unwrap();
        assert!(typed.detail.contains("submitted"));
        assert_eq!(
            text(&app, me, frontend, Some("#status")).unwrap().text,
            "Signed in as ada"
        );
        assert!(
            wait(|done| App::portal_click(&app, me, frontend, "#nope", done))
                .unwrap_err()
                .contains("no element")
        );
        let clicked = wait(|done| App::portal_click(&app, me, frontend, "#next", done)).unwrap();
        assert_eq!(clicked.url, format!("{base}/about.html"));
        assert_eq!(clicked.title.as_deref(), Some("About"));
        let back =
            wait(|done| App::portal_step(&app, me, frontend, PortalStep::Back, done)).unwrap();
        assert_eq!(back.url, format!("{base}/"));
        let forward =
            wait(|done| App::portal_step(&app, me, frontend, PortalStep::Forward, done)).unwrap();
        assert_eq!(forward.url, format!("{base}/about.html"));

        // Screenshot: a real PNG, under Duet's data dir, never elsewhere.
        let shot = wait(|done| App::portal_screenshot(&app, me, frontend, false, done)).unwrap();
        assert!(shot.width > 0 && shot.height > 0);
        let path = PathBuf::from(&shot.path);
        assert!(path.starts_with(app.borrow().portals.base_dir().join("portal-screenshots")));
        assert_eq!(&std::fs::read(&path).unwrap()[..4], b"\x89PNG");

        // Arbitrary JavaScript is privileged: refused for the agent until
        // the portal opts in; the human operator may always.
        let denied = wait(|done| App::portal_evaluate(&app, me, frontend, "document.title", done));
        assert!(denied.unwrap_err().contains("privileged"));
        let human = wait(|done| App::portal_evaluate(&app, None, frontend, "document.title", done));
        assert_eq!(human, Ok(serde_json::json!("About")));
        App::set_portal_allow_scripts(&app, frontend, true);
        let value = wait(|done| {
            App::portal_evaluate(&app, me, frontend, "({sum: 1 + 1, list: [1, 'a']})", done)
        });
        assert_eq!(value, Ok(serde_json::json!({"sum": 2, "list": [1, "a"]})));
        let awaited =
            wait(|done| App::portal_evaluate(&app, me, frontend, "Promise.resolve(5)", done));
        assert_eq!(awaited, Ok(serde_json::json!(5)));
        let thrown =
            wait(|done| App::portal_evaluate(&app, me, frontend, "throw new Error('boom')", done));
        assert!(thrown.unwrap_err().contains("boom"));
        // Statements (not an expression) run once, returning via `return`;
        // a runtime SyntaxError is reported, never retried.
        let statements = wait(|done| {
            App::portal_evaluate(
                &app,
                me,
                frontend,
                "window.duetRuns = (window.duetRuns || 0) + 1; return window.duetRuns;",
                done,
            )
        });
        assert_eq!(statements, Ok(serde_json::json!(1)));
        let runtime_syntax_error = wait(|done| {
            App::portal_evaluate(
                &app,
                me,
                frontend,
                "(window.duetRuns = window.duetRuns + 1, JSON.parse('{'))",
                done,
            )
        });
        assert!(runtime_syntax_error.unwrap_err().contains("SyntaxError"));
        let runs = wait(|done| App::portal_evaluate(&app, me, frontend, "window.duetRuns", done));
        assert_eq!(runs, Ok(serde_json::json!(2)));
        // A page whose Content-Security-Policy forbids scripts (and so
        // `eval`) can still be read and evaluated by Duet.
        navigate(&app, me, frontend, &format!("{base}/csp")).unwrap();
        let strict = wait(|done| App::portal_evaluate(&app, me, frontend, "document.title", done));
        assert_eq!(strict, Ok(serde_json::json!("Strict")));
        assert_eq!(
            text(&app, me, frontend, Some("h1")).unwrap().text,
            "No scripts here"
        );
        // A same-document navigation (fragment) completes promptly rather
        // than waiting for a load event that never comes.
        let started = Instant::now();
        let fragment = navigate(&app, me, frontend, &format!("{base}/csp#section")).unwrap();
        assert_eq!(fragment.url, format!("{base}/csp#section"));
        assert!(started.elapsed() < Duration::from_secs(5));

        // `resource inspect` shows a portal's name to anyone, its URL only
        // to a requester holding ControlPortal.
        let portal_detail = |by| match app.borrow().inspect_resource(frontend, by).unwrap().1 {
            crate::orchestration::resource::ResourceDetail::Portal(summary) => summary,
            other => panic!("expected a portal, got {other:?}"),
        };
        let unauthorized = portal_detail(Some(reviewer));
        assert_eq!((unauthorized.url, unauthorized.controllable), (None, false));
        assert_eq!(unauthorized.name, "Frontend");
        assert!(portal_detail(me).url.is_some());

        // Profile isolation: a cookie set in one portal is visible to that
        // portal (and survives its view being recreated) but not to another
        // portal, nor to a duplicate (which gets its own fresh profile).
        navigate(&app, None, frontend, &format!("{base}/set-cookie?alpha")).unwrap();
        navigate(&app, None, frontend, &format!("{base}/cookie")).unwrap();
        assert_eq!(
            text(&app, None, frontend, Some("#cookie")).unwrap().text,
            "session=alpha"
        );
        navigate(&app, None, docs_b, &format!("{base}/cookie")).unwrap();
        assert_eq!(
            text(&app, None, docs_b, Some("#cookie")).unwrap().text,
            "none"
        );
        app.borrow_mut().portals.remove(frontend);
        let view = App::ensure_portal_view(&app, frontend).unwrap();
        spin_until(|| !view.is_loading());
        navigate(&app, None, frontend, &format!("{base}/cookie")).unwrap();
        assert_eq!(
            text(&app, None, frontend, Some("#cookie")).unwrap().text,
            "session=alpha"
        );
        let mut duplicate = app.borrow().nodes[&frontend]
            .record
            .as_portal()
            .cloned()
            .unwrap();
        isolate_duplicate(&mut duplicate);
        assert_ne!(
            duplicate.profile.id,
            app.borrow().nodes[&frontend]
                .record
                .as_portal()
                .unwrap()
                .profile
                .id
        );

        // "Open in Portal" from the connected agent's terminal navigates its
        // own portal; from an unconnected one it creates a new, connected
        // portal beside it — and only ever as the result of this call.
        assert_eq!(
            App::open_url_in_portal(&app, Some(frontend_agent), &format!("{base}/")),
            Ok(frontend)
        );
        spin_until(|| {
            app.borrow().inspect_portal(None, frontend).unwrap().url == format!("{base}/")
        });
        let created = App::open_url_in_portal(&app, Some(reviewer), &base).unwrap();
        assert!(![frontend, docs_a, docs_b].contains(&created));
        assert!(app.borrow().inspect_portal(Some(reviewer), created).is_ok());
        assert_eq!(
            app.borrow().nodes[&created]
                .record
                .as_portal()
                .unwrap()
                .name,
            "Reviewer preview"
        );

        // Deleting a portal drops its view.
        App::close_node(&app, docs_a);
        assert!(!app.borrow().portals.is_live(docs_a));

        // Like a terminal's PTY, a portal's page survives its workspace
        // being switched away from, and its connected agent can still drive
        // it in the background; switching back shows the same live view.
        let original = app.borrow().workspace_id;
        let view_before = app.borrow().portals.view(frontend).unwrap();
        App::create_workspace(&app, "Other".to_string(), site.path().to_path_buf()).unwrap();
        assert!(!app.borrow().nodes.contains_key(&frontend));
        assert!(app.borrow().portals.is_live(frontend));
        navigate(&app, me, frontend, &format!("{base}/about.html")).unwrap();
        assert_eq!(
            text(&app, me, frontend, Some("h1")).unwrap().text,
            "About page"
        );
        assert!(navigate(&app, Some(reviewer), frontend, &base).is_err());
        App::switch_workspace(&app, original, &adw::ToastOverlay::new());
        let view_after = app.borrow().portals.view(frontend).unwrap();
        assert_eq!(view_before, view_after);
        assert!(view_after.parent().is_some());
        assert_eq!(
            app.borrow().inspect_portal(me, frontend).unwrap().url,
            format!("{base}/about.html")
        );
    }

    /// The Milestone 8 acceptance run. A Frontend agent, connected to a
    /// portal named "Frontend", with no human copying anything between the
    /// browser and the agent:
    ///
    /// 1. resolves `@portal:frontend`;
    /// 2. starts the local app in its own terminal (Duet spots the dev
    ///    server URL in the terminal output and offers it — without
    ///    navigating anything);
    /// 3. navigates the connected portal to it;
    /// 4. reads the page text;
    /// 5. interacts with the page's controls;
    /// 6. captures a screenshot;
    /// 7. changes the app's source;
    /// 8. reloads;
    /// 9. verifies the new state.
    ///
    /// Every step uses the same `App` services `duetctl portal ...` calls.
    /// Needs a display and `python3` (the "local app" is
    /// `python3 -m http.server` started in the agent's terminal):
    /// `cargo test acceptance_milestone_8 -- --ignored --exact`.
    #[test]
    #[ignore = "needs a display"]
    fn acceptance_milestone_8_frontend_agent_verifies_its_app_in_a_portal() {
        if gtk4::init().is_err() {
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let index = project.path().join("index.html");
        let app_source = |heading: &str| {
            format!(
                r#"<!doctype html><html><head><title>Counter app</title></head><body>
<h1 id="heading">{heading}</h1>
<p id="count">Count: 0</p><button id="inc" onclick="const p = document.getElementById('count'); p.textContent = 'Count: ' + (parseInt(p.textContent.slice(7)) + 1);">+1</button>
<input id="name" oninput="document.getElementById('greeting').textContent = 'Hello, ' + this.value"><p id="greeting"></p>
</body></html>"#
            )
        };
        std::fs::write(&index, app_source("Counter")).unwrap();
        let app = test_app(project.path());
        let agent = add_shell_agent(&app, "Frontend", project.path());
        let portal = App::create_portal(&app, Some("Frontend"), "", (600.0, 0.0)).unwrap();
        assert!(App::create_edge(&app, agent, portal));
        let me = Some(agent);

        // 1. Resolve @portal:frontend through the shared resolver.
        let ResolveOutcome::Found { resource } = app
            .borrow()
            .resolve_resource("@portal:frontend", me)
            .unwrap()
        else {
            panic!("@portal:frontend should resolve");
        };
        assert_eq!((resource.kind, resource.id), (ResourceKind::Portal, portal));

        // 2. Start the local app in the agent's own terminal.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(app.borrow_mut().runtime.write_input(
            agent,
            format!("exec python3 -m http.server {port} --bind 127.0.0.1\r").as_bytes(),
        ));
        spin_until(|| {
            app.borrow_mut().pump_output();
            app.borrow()
                .pending_dev_urls
                .iter()
                .any(|(terminal, url)| *terminal == agent && url.contains(&port.to_string()))
        });
        let (_, url) = app
            .borrow()
            .pending_dev_urls
            .iter()
            .find(|(_, url)| url.contains(&port.to_string()))
            .cloned()
            .unwrap();
        assert_eq!(url, format!("http://127.0.0.1:{port}/"));
        // Detected and offered, never navigated by itself.
        assert_eq!(app.borrow().inspect_portal(None, portal).unwrap().url, "");

        // 3. Navigate the connected portal.
        let action = navigate(&app, me, portal, &url).unwrap();
        assert_eq!(action.title.as_deref(), Some("Counter app"));

        // 4. Read the page text.
        let page = text(&app, me, portal, None).unwrap();
        assert!(page.text.contains("Counter"));
        assert!(page.text.contains("Count: 0"));

        // 5. Interact with controls.
        for _ in 0..2 {
            wait(|done| App::portal_click(&app, me, portal, "#inc", done)).unwrap();
        }
        assert_eq!(
            text(&app, me, portal, Some("#count")).unwrap().text,
            "Count: 2"
        );
        wait(|done| App::portal_type(&app, me, portal, "#name", "Ada", false, false, done))
            .unwrap();
        assert_eq!(
            text(&app, me, portal, Some("#greeting")).unwrap().text,
            "Hello, Ada"
        );

        // 6. Screenshot.
        let shot = wait(|done| App::portal_screenshot(&app, me, portal, false, done)).unwrap();
        assert!(Path::new(&shot.path).exists());

        // 7. Change the source.
        std::fs::write(&index, app_source("Counter v2")).unwrap();

        // 8. Reload.
        let reloaded =
            wait(|done| App::portal_step(&app, me, portal, PortalStep::Reload, done)).unwrap();
        assert_eq!(reloaded.url, url);

        // 9. Verify the new state: the new heading, and a fresh page.
        assert_eq!(
            text(&app, me, portal, Some("#heading")).unwrap().text,
            "Counter v2"
        );
        assert_eq!(
            text(&app, me, portal, Some("#count")).unwrap().text,
            "Count: 0"
        );

        crate::environment::terminate(
            &mut app.borrow_mut().runtime,
            agent,
            EnvironmentKind::LocalPty,
        );
    }
}
