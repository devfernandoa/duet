//! Milestone 7.6: the GTK-facing half of delegated canvas control. The
//! grant *state* lives in `orchestration::control_grant::
//! DelegatedControlService` (pure, no GTK); this file owns the one thing
//! that must stay human-only — presenting the approval dialog and acting
//! on its response — plus the small per-agent "control granted" revoke
//! affordance. Nothing in `src/control.rs` calls `approve`/`deny` directly;
//! only the dialog's `connect_response` closure below does, which is only
//! ever reachable by an actual button press in the trusted GTK window.

use super::App;
use super::files::window_of;
use crate::orchestration::control_grant::{ControlCapability, PendingGrantRequest};
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use uuid::Uuid;

impl App {
    /// `duetctl control request`'s service: records the request and shows
    /// the human-approval dialog. Returns immediately (the dialog is
    /// answered later, out of band — see `control_status`), matching the
    /// control socket's 5-second synchronous reply budget.
    pub fn request_control_grant(
        app: &Rc<RefCell<App>>,
        requested_by: Uuid,
        capabilities: BTreeSet<ControlCapability>,
        reason: Option<String>,
    ) -> Uuid {
        let workspace_id = app.borrow().workspace_id;
        let request_id = app.borrow_mut().control.request_grant(
            requested_by,
            workspace_id,
            capabilities,
            reason,
        );
        App::present_control_grant_dialog(app, request_id);
        request_id
    }

    /// `duetctl control status`'s service: the capabilities currently
    /// granted to `agent_id` in the active workspace (empty if none/denied/
    /// still pending).
    pub fn control_status(app: &Rc<RefCell<App>>, agent_id: Uuid) -> BTreeSet<ControlCapability> {
        let app_ref = app.borrow();
        app_ref
            .control
            .granted_capabilities(app_ref.workspace_id, agent_id)
    }

    /// Only ever called from the dialog's "Allow" response below.
    pub(super) fn approve_control_grant(
        app: &Rc<RefCell<App>>,
        request_id: Uuid,
    ) -> Result<PendingGrantRequest, String> {
        let request = app.borrow_mut().control.approve(request_id)?;
        let name = App::agent_display_name(app, request.agent_id);
        App::notify(app, &format!("Canvas control granted to {name}"));
        Ok(request)
    }

    /// Only ever called from the dialog's "Deny" response below.
    pub(super) fn deny_control_grant(
        app: &Rc<RefCell<App>>,
        request_id: Uuid,
    ) -> Result<PendingGrantRequest, String> {
        app.borrow_mut().control.deny(request_id)
    }

    /// The per-agent revoke action (Terminal node menu) and future UI call
    /// this directly — it is not a response to any dialog, so it is safe
    /// to also expose as an ordinary `App::` method.
    pub fn revoke_control_grant(app: &Rc<RefCell<App>>, agent_id: Uuid) -> bool {
        let workspace_id = app.borrow().workspace_id;
        let revoked = app.borrow_mut().control.revoke(workspace_id, agent_id);
        if revoked {
            let name = App::agent_display_name(app, agent_id);
            App::notify(app, &format!("Canvas control revoked for {name}"));
        }
        revoked
    }

    fn agent_display_name(app: &Rc<RefCell<App>>, agent_id: Uuid) -> String {
        app.borrow()
            .agent_registry()
            .get_agent(agent_id)
            .map(|identity| identity.name.clone())
            .unwrap_or_else(|| agent_id.to_string())
    }

    /// §5's approval dialog: non-destructive capabilities listed plainly,
    /// any destructive ask (today, only `RemoveCanvasNodes`) visually
    /// separated under "Also requested", never hidden in fine print.
    fn present_control_grant_dialog(app: &Rc<RefCell<App>>, request_id: Uuid) {
        let (requester_name, capabilities, reason, workspace_name) = {
            let app_ref = app.borrow();
            let Some(request) = app_ref.control.pending_request(request_id) else {
                return;
            };
            let requester_name = app_ref
                .agent_registry()
                .get_agent(request.agent_id)
                .map(|identity| identity.name.clone())
                .unwrap_or_else(|| request.agent_id.to_string());
            (
                requester_name,
                request.capabilities.clone(),
                request.reason.clone(),
                app_ref.workspace_name.clone(),
            )
        };
        let toast_overlay = App::toaster(app);
        let Some(window) = window_of(&toast_overlay) else {
            return;
        };

        let (non_destructive, destructive): (Vec<&ControlCapability>, Vec<&ControlCapability>) =
            capabilities.iter().partition(|cap| !cap.is_destructive());

        let mut body = String::from("Requested access:\n");
        for cap in &non_destructive {
            body.push_str(&format!("  \u{2022} {}\n", cap.label()));
        }
        if !destructive.is_empty() {
            body.push_str("\nAlso requested:\n");
            for cap in &destructive {
                body.push_str(&format!("  \u{2022} {}\n", cap.label()));
            }
        }
        body.push_str(&format!("\nWorkspace: {workspace_name}"));
        if let Some(reason) = &reason {
            body.push_str(&format!("\n\nReason: {reason}"));
        }

        let dialog = adw::MessageDialog::new(
            Some(&window),
            Some(&format!("{requester_name} wants to control this workspace")),
            Some(&body),
        );
        dialog.add_response("deny", "Deny");
        dialog.add_response("allow", "Allow");
        if !destructive.is_empty() {
            dialog.set_response_appearance("allow", adw::ResponseAppearance::Destructive);
        }
        dialog.set_default_response(Some("deny"));
        dialog.set_close_response("deny");
        dialog.connect_response(None, {
            let app = Rc::clone(app);
            move |_, response| {
                if response == "allow" {
                    let _ = App::approve_control_grant(&app, request_id);
                } else {
                    let _ = App::deny_control_grant(&app, request_id);
                }
            }
        });
        dialog.present();
    }
}
