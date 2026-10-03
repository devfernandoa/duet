//! Owns every live Portal's WebKit view and every profile's network
//! session — the runtime half of a Portal, the way `runtime::SessionRuntime`
//! is the runtime half of a Terminal. Keyed by the portal node's stable id;
//! nothing here is ever persisted (CLAUDE.md: "WebKit views" are runtime
//! objects). The persisted half is `model::PortalPayload`; the widget that
//! shows a view on the canvas is `node_portal::PortalNode`, which only
//! *borrows* a view from here — so, like a PTY, a portal's page (its
//! history, its in-memory state, a half-filled form) survives its
//! workspace being switched away from, and an agent can keep driving a
//! portal whose card isn't on screen.
//!
//! Storage isolation lives at the `NetworkSession` level: each
//! `PortalProfile` id gets its own session (its own cookie jar, local
//! storage, cache), rooted at `orchestration::portal::profile_dirs` —
//! derived from the profile id, never a stored path — or entirely in
//! memory for an ephemeral profile.
//!
//! The async helpers at the bottom (`run_script`, `load_and_wait`,
//! `capture`) are the only places WebKit's callback-style API is touched;
//! `app::portals` builds the `PortalService` operations from them.

use crate::model::PortalProfile;
use crate::orchestration::portal::profile_dirs;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use uuid::Uuid;
use webkit6::prelude::*;

/// The isolated JavaScript world Duet's own page scripts (text, click,
/// type) run in: they see the same DOM as the page, but none of the page's
/// own JavaScript globals, so a page can't spoof or observe them. Agent
/// `evaluate` scripts deliberately run in the page's main world instead —
/// inspecting the app's own state is the point of them.
pub const DUET_WORLD: &str = "duet";

/// How long a navigation/reload waits for the page to finish loading
/// before answering with a timeout.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// A navigation that hasn't started a load this long after being asked
/// for was same-document (a `#fragment`, or a single-page app's history
/// change) — it is already complete, and no load event is ever coming.
const SAME_DOCUMENT_GRACE: Duration = Duration::from_millis(300);

pub struct PortalRuntime {
    /// Duet's data directory; profiles and screenshots live under it.
    base_dir: PathBuf,
    views: HashMap<Uuid, webkit6::WebView>,
    sessions: HashMap<Uuid, webkit6::NetworkSession>,
}

impl PortalRuntime {
    pub fn new(base_dir: PathBuf) -> PortalRuntime {
        PortalRuntime {
            base_dir,
            views: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    pub fn base_dir(&self) -> &std::path::Path {
        &self.base_dir
    }

    pub fn view(&self, id: Uuid) -> Option<webkit6::WebView> {
        self.views.get(&id).cloned()
    }

    pub fn is_live(&self, id: Uuid) -> bool {
        self.views.contains_key(&id)
    }

    /// The network session for `profile`, created on first use.
    fn session(&mut self, profile: &PortalProfile) -> webkit6::NetworkSession {
        if let Some(session) = self.sessions.get(&profile.id) {
            return session.clone();
        }
        let session = match profile_dirs(&self.base_dir, profile) {
            Some((data, cache)) => {
                // Cookies and storage are credentials: the profile's own
                // directory is owner-only, not just its leaves.
                for dir in [&data, &cache] {
                    let _ = std::fs::create_dir_all(dir);
                    restrict_to_owner(dir);
                }
                if let Some(root) = data.parent() {
                    restrict_to_owner(root);
                }
                webkit6::NetworkSession::new(data.to_str(), cache.to_str())
            }
            None => webkit6::NetworkSession::new_ephemeral(),
        };
        self.sessions.insert(profile.id, session.clone());
        session
    }

    /// Creates portal `id`'s view (loading `url`, if any) unless it already
    /// exists. Returns the view and whether it was newly created, so the
    /// caller can wire its signals exactly once.
    pub fn ensure(
        &mut self,
        id: Uuid,
        profile: &PortalProfile,
        url: &str,
    ) -> (webkit6::WebView, bool) {
        if let Some(view) = self.views.get(&id) {
            return (view.clone(), false);
        }
        let session = self.session(profile);
        let view = webkit6::WebView::builder()
            .network_session(&session)
            .hexpand(true)
            .vexpand(true)
            .build();
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
            settings.set_javascript_can_open_windows_automatically(false);
            settings.set_enable_developer_extras(true);
        }
        // `target=_blank` links and `window.open` load in the same portal:
        // a portal is one page on the canvas, not a window manager.
        view.connect_create(|view, action| {
            if let Some(uri) = action.clone().request().and_then(|request| request.uri()) {
                view.load_uri(&uri);
            }
            None
        });
        // Anything WebKit can't display would otherwise be downloaded to
        // disk; a portal shows pages, it doesn't save files nobody asked
        // for.
        view.connect_decide_policy(|_, decision, kind| {
            if kind == webkit6::PolicyDecisionType::Response
                && let Some(response) = decision.downcast_ref::<webkit6::ResponsePolicyDecision>()
                && !response.is_mime_type_supported()
            {
                decision.ignore();
                return true;
            }
            false
        });
        if !url.is_empty() {
            view.load_uri(url);
        }
        self.views.insert(id, view.clone());
        (view, true)
    }

    /// Drops portal `id`'s view (stopping any load). Its profile's session
    /// and on-disk data are kept: undoing the deletion brings the portal
    /// back with its cookies intact.
    pub fn remove(&mut self, id: Uuid) {
        if let Some(view) = self.views.remove(&id) {
            view.stop_loading();
            if let Some(parent) = view.parent().and_downcast::<gtk4::Box>() {
                parent.remove(&view);
            }
        }
    }

    /// Clears every kind of website data (cookies, storage, cache) a
    /// profile holds.
    pub fn clear_profile_data(
        &mut self,
        profile: &PortalProfile,
        done: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let session = self.session(profile);
        let Some(manager) = session.website_data_manager() else {
            done(Err("this profile has no website data manager".to_string()));
            return;
        };
        manager.clear(
            webkit6::WebsiteDataTypes::ALL,
            glib::TimeSpan::from_seconds(0),
            None::<&gtk4::gio::Cancellable>,
            move |result| done(result.map_err(|error| error.to_string())),
        );
    }
}

#[cfg(unix)]
pub(crate) fn restrict_to_owner(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
pub(crate) fn restrict_to_owner(_dir: &std::path::Path) {}

/// A callback that may be completed from whichever of several signal
/// handlers / timeouts fires first, exactly once.
type Once<T> = Rc<RefCell<Option<Box<dyn FnOnce(T)>>>>;

fn once<T: 'static>(f: impl FnOnce(T) + 'static) -> Once<T> {
    Rc::new(RefCell::new(Some(Box::new(f))))
}

fn complete<T>(slot: &Once<T>, value: T) {
    let callback = slot.borrow_mut().take();
    if let Some(callback) = callback {
        callback(value);
    }
}

/// Runs `body` (a function body; `return` its result, `await` allowed) in
/// `view`, in Duet's isolated world when `isolated`, and hands back the
/// returned value as a string (strings as-is, anything else as JSON).
pub fn run_script(
    view: &webkit6::WebView,
    body: &str,
    isolated: bool,
    done: impl FnOnce(Result<String, String>) + 'static,
) {
    view.call_async_javascript_function(
        body,
        None,
        isolated.then_some(DUET_WORLD),
        None,
        None::<&gtk4::gio::Cancellable>,
        move |result| {
            done(match result {
                Ok(value) if value.is_string() => Ok(value.to_str().to_string()),
                Ok(value) => Ok(value
                    .to_json(0)
                    .map(|json| json.to_string())
                    .unwrap_or_else(|| "null".to_string())),
                Err(error) => Err(format!("the page script failed: {error}")),
            })
        },
    );
}

/// Starts a load with `start` (load a URL, reload, go back...) and calls
/// `done` once the page has finished loading, failed, or `timeout` passed.
/// Signal handlers are connected *before* `start` runs, so even a load that
/// completes instantly is observed.
pub fn load_and_wait(
    view: &webkit6::WebView,
    timeout: Duration,
    start: impl FnOnce(&webkit6::WebView),
    done: impl FnOnce(Result<(), String>) + 'static,
) {
    let slot = once(done);
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let handlers: Rc<RefCell<Vec<glib::SignalHandlerId>>> = Rc::new(RefCell::new(Vec::new()));
    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    let finish = {
        let view = view.clone();
        let slot = slot.clone();
        let handlers = handlers.clone();
        let timer = timer.clone();
        move |result: Result<(), String>| {
            for handler in handlers.borrow_mut().drain(..) {
                view.disconnect(handler);
            }
            if let Some(timer) = timer.borrow_mut().take() {
                timer.remove();
            }
            complete(&slot, result);
        }
    };
    let finish = Rc::new(finish);

    let failed = view.connect_load_failed({
        let failure = failure.clone();
        move |_view, _event, uri, error| {
            // Cancelled loads (a newer navigation replaced this one) are
            // not failures of what the caller asked for.
            if !error.matches(webkit6::NetworkError::Cancelled) {
                *failure.borrow_mut() = Some(format!("couldn't load {uri}: {error}"));
            }
            false
        }
    });
    let started = Rc::new(std::cell::Cell::new(false));
    let changed = view.connect_load_changed({
        let finish = finish.clone();
        let failure = failure.clone();
        let started = started.clone();
        move |_view, event| {
            if event == webkit6::LoadEvent::Started {
                started.set(true);
            }
            if event == webkit6::LoadEvent::Finished {
                let result = match failure.borrow_mut().take() {
                    Some(error) => Err(error),
                    None => Ok(()),
                };
                // Deferred: never disconnect a handler from inside its own
                // emission.
                let finish = finish.clone();
                glib::idle_add_local_once(move || finish(result));
            }
        }
    });
    handlers.borrow_mut().extend([failed, changed]);
    *timer.borrow_mut() = Some(glib::timeout_add_local_once(timeout, {
        let finish = finish.clone();
        let timer = timer.clone();
        move || {
            timer.borrow_mut().take();
            finish(Err(format!(
                "the page was still loading after {}s",
                timeout.as_secs()
            )))
        }
    }));
    start(view);
    glib::timeout_add_local_once(SAME_DOCUMENT_GRACE, {
        let view = view.clone();
        move || {
            if !started.get() && !view.is_loading() {
                finish(Ok(()));
            }
        }
    });
}

/// Captures what the portal shows (the visible viewport when its card is
/// on screen, the whole document otherwise — an off-screen view has no
/// meaningful viewport) as a texture.
pub fn capture(
    view: &webkit6::WebView,
    full_page: bool,
    done: impl FnOnce(Result<gtk4::gdk::Texture, String>) + 'static,
) {
    let region = if full_page || !view.is_mapped() {
        webkit6::SnapshotRegion::FullDocument
    } else {
        webkit6::SnapshotRegion::Visible
    };
    view.snapshot(
        region,
        webkit6::SnapshotOptions::NONE,
        None::<&gtk4::gio::Cancellable>,
        move |result| done(result.map_err(|error| format!("couldn't capture the page: {error}"))),
    );
}
