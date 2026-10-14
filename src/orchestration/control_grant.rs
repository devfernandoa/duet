//! Milestone 7.6: delegated canvas-control grants. An orchestrator Agent may
//! REQUEST a set of capabilities over the canvas; only a human action in the
//! GTK app (never anything reachable through the control socket — see
//! `App::approve_control_grant`/`deny_control_grant` in
//! `app/delegated_control.rs`) may approve or deny that request. Grants are
//! pure runtime state — never part of `Store`/`WorkspaceRecord` — scoped to
//! the exact `(workspace_id, agent_id)` pair that asked, and vanish on
//! revoke, that agent stopping, its workspace closing/unloading, or Duet
//! exiting (nothing here is persisted, so exit clears everything for free).
//!
//! Trust boundary: a grant *request*'s `requested_by: Option<Uuid>` (like
//! every other acting-agent-identified field in this protocol, e.g.
//! `AgentsCreate`/`AgentsRemove`/`require_manager`) is filled in by
//! `duetctl` from `$DUET_AGENT_ID` before it ever reaches the control
//! socket. The socket itself (`handle_connection` in `src/control.rs`) does
//! no peer authentication, so a process writing JSON to it directly
//! (bypassing `duetctl`) could omit `requested_by` entirely — read as the
//! trusted human — or claim to be a different agent. This is a pre-existing
//! trust model, not something this milestone changes, but delegated control
//! makes it the sole enforcement point for canvas mutation, not just
//! agent-roster management. Socket-level peer authentication (e.g. binding
//! the acting agent to the connection itself rather than trusting a payload
//! field) is a hardening-milestone concern, not addressed here.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

/// A single delegated-control authority. Named to match CLAUDE.md
/// Milestone 7.6 §2's recommended capability set exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlCapability {
    SpawnAgents,
    CreateCanvasNodes,
    MoveCanvasNodes,
    ResizeCanvasNodes,
    ConfigureCanvasNodes,
    ConnectCanvasNodes,
    /// Deleting non-agent canvas nodes only — terminating an Agent stays
    /// behind the existing `App::require_manager`-gated `remove_agent`,
    /// never this capability (§2's "must not automatically grant...
    /// terminating existing user Agents").
    RemoveCanvasNodes,
}

impl ControlCapability {
    /// What a plain "Canvas Control" approval grants with no explicit
    /// capability list — everything except `RemoveCanvasNodes`, which
    /// always needs explicit inclusion (§2).
    pub const DEFAULT_SET: [ControlCapability; 6] = [
        ControlCapability::SpawnAgents,
        ControlCapability::CreateCanvasNodes,
        ControlCapability::MoveCanvasNodes,
        ControlCapability::ResizeCanvasNodes,
        ControlCapability::ConfigureCanvasNodes,
        ControlCapability::ConnectCanvasNodes,
    ];

    pub fn is_destructive(self) -> bool {
        matches!(self, ControlCapability::RemoveCanvasNodes)
    }

    /// Plain-language label for the approval dialog and `duetctl control
    /// status` output.
    pub fn label(self) -> &'static str {
        match self {
            ControlCapability::SpawnAgents => "Create and start coding agents",
            ControlCapability::CreateCanvasNodes => "Create canvas items",
            ControlCapability::MoveCanvasNodes => "Move canvas items",
            ControlCapability::ResizeCanvasNodes => "Resize canvas items",
            ControlCapability::ConfigureCanvasNodes => "Rename and configure canvas items",
            ControlCapability::ConnectCanvasNodes => "Connect canvas items",
            ControlCapability::RemoveCanvasNodes => "Delete canvas items",
        }
    }
}

/// A grant request awaiting a human decision.
#[derive(Debug, Clone)]
pub struct PendingGrantRequest {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub workspace_id: Uuid,
    pub capabilities: BTreeSet<ControlCapability>,
    pub reason: Option<String>,
}

/// Owns every pending request and active grant. Lives on `App` as plain
/// runtime state (like `SessionRuntime`/`MessageBus`) — never serialized.
#[derive(Default)]
pub struct DelegatedControlService {
    pending: HashMap<Uuid, PendingGrantRequest>,
    grants: HashMap<(Uuid, Uuid), BTreeSet<ControlCapability>>,
}

impl DelegatedControlService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a request; returns its id. Does not notify anyone — the
    /// caller (`App::request_control_grant`) is responsible for showing the
    /// approval dialog.
    pub fn request_grant(
        &mut self,
        agent_id: Uuid,
        workspace_id: Uuid,
        capabilities: BTreeSet<ControlCapability>,
        reason: Option<String>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        self.pending.insert(
            id,
            PendingGrantRequest {
                id,
                agent_id,
                workspace_id,
                capabilities,
                reason,
            },
        );
        id
    }

    pub fn pending_request(&self, request_id: Uuid) -> Option<&PendingGrantRequest> {
        self.pending.get(&request_id)
    }

    /// Grants exactly the requested capabilities. Only ever called from a
    /// GTK dialog response closure — see `app/delegated_control.rs`.
    pub fn approve(&mut self, request_id: Uuid) -> Result<PendingGrantRequest, String> {
        let request = self
            .pending
            .remove(&request_id)
            .ok_or_else(|| "no such pending control request".to_string())?;
        self.grants.insert(
            (request.workspace_id, request.agent_id),
            request.capabilities.clone(),
        );
        Ok(request)
    }

    pub fn deny(&mut self, request_id: Uuid) -> Result<PendingGrantRequest, String> {
        self.pending
            .remove(&request_id)
            .ok_or_else(|| "no such pending control request".to_string())
    }

    /// Returns whether a grant actually existed to remove.
    pub fn revoke(&mut self, workspace_id: Uuid, agent_id: Uuid) -> bool {
        self.grants.remove(&(workspace_id, agent_id)).is_some()
    }

    /// Drops every grant and pending request scoped to `workspace_id` —
    /// called when that workspace closes/unloads/detaches, since every
    /// agent in it stops running too.
    pub fn clear_workspace(&mut self, workspace_id: Uuid) {
        self.grants.retain(|(ws, _), _| *ws != workspace_id);
        self.pending
            .retain(|_, request| request.workspace_id != workspace_id);
    }

    pub fn has_capability(
        &self,
        workspace_id: Uuid,
        agent_id: Uuid,
        capability: ControlCapability,
    ) -> bool {
        self.grants
            .get(&(workspace_id, agent_id))
            .is_some_and(|caps| caps.contains(&capability))
    }

    pub fn has_any_grant(&self, workspace_id: Uuid, agent_id: Uuid) -> bool {
        self.grants.contains_key(&(workspace_id, agent_id))
    }

    pub fn granted_capabilities(
        &self,
        workspace_id: Uuid,
        agent_id: Uuid,
    ) -> BTreeSet<ControlCapability> {
        self.grants
            .get(&(workspace_id, agent_id))
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(set: &[ControlCapability]) -> BTreeSet<ControlCapability> {
        set.iter().copied().collect()
    }

    #[test]
    fn request_then_approve_grants_exactly_the_requested_capabilities() {
        let mut service = DelegatedControlService::new();
        let agent = Uuid::new_v4();
        let workspace = Uuid::new_v4();
        let requested = caps(&[
            ControlCapability::SpawnAgents,
            ControlCapability::CreateCanvasNodes,
        ]);
        let request_id = service.request_grant(
            agent,
            workspace,
            requested.clone(),
            Some("set up a team".into()),
        );

        assert!(!service.has_capability(workspace, agent, ControlCapability::SpawnAgents));
        let approved = service.approve(request_id).unwrap();
        assert_eq!(approved.capabilities, requested);
        assert!(service.has_capability(workspace, agent, ControlCapability::SpawnAgents));
        assert!(!service.has_capability(workspace, agent, ControlCapability::RemoveCanvasNodes));
    }

    #[test]
    fn deny_never_grants_anything() {
        let mut service = DelegatedControlService::new();
        let agent = Uuid::new_v4();
        let workspace = Uuid::new_v4();
        let request_id = service.request_grant(
            agent,
            workspace,
            caps(&[ControlCapability::SpawnAgents]),
            None,
        );
        service.deny(request_id).unwrap();
        assert!(!service.has_capability(workspace, agent, ControlCapability::SpawnAgents));
        assert!(service.pending_request(request_id).is_none());
    }

    #[test]
    fn approving_twice_fails_the_second_time() {
        let mut service = DelegatedControlService::new();
        let request_id = service.request_grant(
            Uuid::new_v4(),
            Uuid::new_v4(),
            caps(&[ControlCapability::SpawnAgents]),
            None,
        );
        assert!(service.approve(request_id).is_ok());
        assert!(service.approve(request_id).is_err());
    }

    #[test]
    fn a_grant_is_scoped_to_its_own_agent_and_workspace() {
        let mut service = DelegatedControlService::new();
        let agent_a = Uuid::new_v4();
        let agent_b = Uuid::new_v4();
        let workspace_a = Uuid::new_v4();
        let workspace_b = Uuid::new_v4();
        let request_id = service.request_grant(
            agent_a,
            workspace_a,
            caps(&[ControlCapability::MoveCanvasNodes]),
            None,
        );
        service.approve(request_id).unwrap();

        assert!(service.has_capability(workspace_a, agent_a, ControlCapability::MoveCanvasNodes));
        assert!(!service.has_capability(workspace_a, agent_b, ControlCapability::MoveCanvasNodes));
        assert!(!service.has_capability(workspace_b, agent_a, ControlCapability::MoveCanvasNodes));
    }

    #[test]
    fn revoke_removes_an_active_grant() {
        let mut service = DelegatedControlService::new();
        let agent = Uuid::new_v4();
        let workspace = Uuid::new_v4();
        let request_id = service.request_grant(
            agent,
            workspace,
            caps(&[ControlCapability::SpawnAgents]),
            None,
        );
        service.approve(request_id).unwrap();
        assert!(service.revoke(workspace, agent));
        assert!(!service.has_capability(workspace, agent, ControlCapability::SpawnAgents));
        assert!(!service.revoke(workspace, agent));
    }

    #[test]
    fn clear_workspace_drops_its_grants_and_pending_requests_only() {
        let mut service = DelegatedControlService::new();
        let agent = Uuid::new_v4();
        let workspace_a = Uuid::new_v4();
        let workspace_b = Uuid::new_v4();
        let grant_a = service.request_grant(
            agent,
            workspace_a,
            caps(&[ControlCapability::SpawnAgents]),
            None,
        );
        service.approve(grant_a).unwrap();
        let grant_b = service.request_grant(
            agent,
            workspace_b,
            caps(&[ControlCapability::SpawnAgents]),
            None,
        );
        service.approve(grant_b).unwrap();

        service.clear_workspace(workspace_a);

        assert!(!service.has_capability(workspace_a, agent, ControlCapability::SpawnAgents));
        assert!(service.has_capability(workspace_b, agent, ControlCapability::SpawnAgents));
    }

    #[test]
    fn default_set_excludes_remove_canvas_nodes() {
        assert!(!ControlCapability::DEFAULT_SET.contains(&ControlCapability::RemoveCanvasNodes));
        assert!(ControlCapability::RemoveCanvasNodes.is_destructive());
        assert!(!ControlCapability::SpawnAgents.is_destructive());
    }

    /// Global Constraints (see the milestone plan): no `ControlRequest`
    /// handler in `src/control.rs` may ever cause a delegated-control grant
    /// to become approved or denied — that must only happen from the
    /// human's own approval dialog in `src/app/delegated_control.rs`. This
    /// is enforced structurally here by scanning `control.rs`'s source text
    /// for the two function names that would do it. Plain string check, no
    /// GTK involved, so it runs in every `cargo test`, not just `--ignored`.
    #[test]
    fn control_rs_never_approves_or_denies_a_control_grant() {
        let source = include_str!("../control.rs");
        for forbidden in ["approve_control_grant", "deny_control_grant"] {
            assert!(
                !source.contains(forbidden),
                "src/control.rs must never call `{forbidden}` — approving/denying a \
                 delegated-control grant must only happen from the human's own \
                 approval dialog in src/app/delegated_control.rs"
            );
        }
    }
}
