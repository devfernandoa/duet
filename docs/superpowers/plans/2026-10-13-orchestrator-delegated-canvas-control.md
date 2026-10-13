# Orchestrator Delegated Canvas Control Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an authorized orchestrator Agent spawn agents and create/move/resize/configure/connect canvas nodes through `duetctl`, but only after a human approves an explicit, scoped capability grant through a Duet GTK dialog.

**Architecture:** Extend the existing `App::` service layer (not a new layer) with a pure, GTK-free `DelegatedControlService` (capability grants keyed by `(workspace_id, agent_id)`, in-memory only — never persisted) living on `App`. Every privileged `App::` method gets a `requested_by: Option<Uuid>` capability check at the same boundary `require_manager` already uses. New `ControlRequest`/`ControlResponse` variants extend the existing control-socket protocol through its single `handle_request` dispatch chokepoint. The human-approval dialog is a new `adw::MessageDialog`, shown only from a GTK response-signal closure — no code path reachable from the socket can approve or deny a request.

**Tech Stack:** Rust, GTK4/libadwaita (`adw::MessageDialog`), serde/serde_json (control-socket wire format), existing `anyhow`/`Result<_, String>` error conventions.

**Spec:** The Milestone 7.6 "Orchestrator Delegated Canvas Control" spec is the numbered-section document the user pasted in-conversation (INSPECT CURRENT ARCHITECTURE … ORCHESTRATOR ROLE). No standalone spec file exists in the repo; `.claude/steps.md` and `.claude/instructions.md` are both stale (predate milestones 4-8/7.5 that already shipped) and must not be treated as ground truth — git log and the current source tree are ground truth. This plan is the spec's sole executable artifact; section numbers below (§2, §5, …) refer to that pasted document.

## Global Constraints

- Grants are scoped to `(requesting agent id, workspace id)` and are **never persisted** — pure in-memory `App` runtime state, gone on revoke, agent-stop, workspace-close/unload, or process exit (§3).
- A default "Canvas Control" grant covers `SpawnAgents, CreateCanvasNodes, MoveCanvasNodes, ResizeCanvasNodes, ConfigureCanvasNodes, ConnectCanvasNodes` — **never** `RemoveCanvasNodes`, which always needs explicit inclusion and is visually distinguished in the approval dialog (§2, §5).
- No `ControlRequest` variant may ever cause a grant to become approved or denied — only a GTK dialog response-signal closure may call `approve`/`deny` (§6). This is enforced structurally: write a test asserting the only two call sites of `DelegatedControlService::approve`/`deny` are inside `src/app/delegated_control.rs`'s dialog closure, not `src/control.rs`.
- Every privileged mutation checks authorization at the `App::` method boundary, not only in `control.rs`'s CLI parsing (§7), mirroring `App::require_manager`.
- Reuse existing services — do not duplicate agent spawning (`App::create_session`/`create_agent`), node creation (`App::add_simple_node`), or capability-defaulting on connect (`App::create_edge`'s existing kind-pair table) (§8, §11, §16).
- A locked node is never moved/resized by delegated control; no "unlock" capability is introduced in this pass (§15).
- Agent/terminal *termination* stays exclusively behind the existing `require_manager`-gated `App::remove_agent`/`AgentsRemove` — `RemoveCanvasNodes` only ever covers non-agent nodes (Note/Text/FileTree/Portal/Drawing/Group) (§2's "must not automatically grant... terminating existing user Agents").
- Out of scope for this pass (explicitly optional/skippable per the spec, flagged here rather than built speculatively): batch/transactional operations (§14 — "optional... do not introduce a complex transaction engine"), workspace-bounds/overlap-detection reporting beyond `find-space` (§13 — "where practical"), a persisted/partial-capability permission editor (§5 — "do not create an overly complicated permission editor").

---

## File Structure

| File | Change |
|---|---|
| `src/orchestration/control_grant.rs` | **Create.** `ControlCapability` enum, `PendingGrantRequest`, `DelegatedControlService` — pure domain/service code, no GTK. |
| `src/orchestration/mod.rs` | Add `pub mod control_grant;` + re-export. |
| `src/app.rs` | Add `control: DelegatedControlService` field to `App` + init in `App::new`; add `require_control_capability`; hook revocation into `close_node`, `detach_active_workspace`, `teardown_active_workspace`, `unload_workspace`; extend `create_agent`/`create_session` (position/size/initial prompt); extend `create_edge` (requested_by); add `move_node`, `resize_node`, `create_canvas_node`, `configure_node`, `remove_canvas_node`, `list_canvas_nodes`, `inspect_canvas_node`, `find_canvas_space`; make `add_simple_node`/`window_of`-equivalent reachable from the new submodule; `mod delegated_control;`. |
| `src/app/delegated_control.rs` | **Create.** GTK-side glue: `present_control_grant_dialog`, `App::approve_control_grant`/`deny_control_grant`/`request_control_grant`/`revoke_control_grant`/`control_status`, the Terminal-node revoke menu item. |
| `src/app/files.rs` | Change `fn window_of` to `pub(super) fn window_of` (no behavior change) so `delegated_control.rs` can reuse it. |
| `src/control.rs` | New `ControlRequest`/`ControlResponse` variants: `ControlRequest`(request grant)/`ControlStatus`; `CanvasList`/`CanvasInspect`/`CanvasMove`/`CanvasResize`/`CanvasCreate`/`CanvasConfigure`/`CanvasConnect`/`CanvasRemove`/`CanvasFindSpace`; extend `AgentsCreate` with optional `x`,`y`,`width`,`height`,`task`; new CLI argv parsers + `handle_request` arms. |
| `src/orchestration/skill.rs` | Extend `SKILL_MD` with a delegated-control workflow section (§19); add asserting tests; keep every existing asserted substring intact. |

---

### Task 1: Delegated-control domain model (`ControlCapability`, `DelegatedControlService`)

**Files:**
- Create: `src/orchestration/control_grant.rs`
- Modify: `src/orchestration/mod.rs`

**Interfaces:**
- Produces: `ControlCapability` (7 variants: `SpawnAgents, CreateCanvasNodes, MoveCanvasNodes, ResizeCanvasNodes, ConfigureCanvasNodes, ConnectCanvasNodes, RemoveCanvasNodes`), `ControlCapability::DEFAULT_SET: [ControlCapability; 6]` (excludes `RemoveCanvasNodes`), `ControlCapability::is_destructive(self) -> bool`, `ControlCapability::label(self) -> &'static str`; `PendingGrantRequest { id, agent_id, workspace_id, capabilities: BTreeSet<ControlCapability>, reason: Option<String> }`; `DelegatedControlService::{new, request_grant, pending_request, approve, deny, revoke, clear_workspace, has_capability, has_any_grant, granted_capabilities}`.

- [ ] **Step 1: Write failing unit tests for the service**

```rust
// bottom of src/orchestration/control_grant.rs
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
        let requested = caps(&[ControlCapability::SpawnAgents, ControlCapability::CreateCanvasNodes]);
        let request_id = service.request_grant(agent, workspace, requested.clone(), Some("set up a team".into()));

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
        let request_id = service.request_grant(agent, workspace, caps(&[ControlCapability::SpawnAgents]), None);
        service.deny(request_id).unwrap();
        assert!(!service.has_capability(workspace, agent, ControlCapability::SpawnAgents));
        assert!(service.pending_request(request_id).is_none());
    }

    #[test]
    fn approving_twice_fails_the_second_time() {
        let mut service = DelegatedControlService::new();
        let request_id = service.request_grant(Uuid::new_v4(), Uuid::new_v4(), caps(&[ControlCapability::SpawnAgents]), None);
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
        let request_id = service.request_grant(agent_a, workspace_a, caps(&[ControlCapability::MoveCanvasNodes]), None);
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
        let request_id = service.request_grant(agent, workspace, caps(&[ControlCapability::SpawnAgents]), None);
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
        let grant_a = service.request_grant(agent, workspace_a, caps(&[ControlCapability::SpawnAgents]), None);
        service.approve(grant_a).unwrap();
        let grant_b = service.request_grant(agent, workspace_b, caps(&[ControlCapability::SpawnAgents]), None);
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
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib orchestration::control_grant`
Expected: FAIL to compile — module doesn't exist yet.

- [ ] **Step 3: Implement the service**

```rust
//! Milestone 7.6: delegated canvas-control grants. An orchestrator Agent may
//! REQUEST a set of capabilities over the canvas; only a human action in the
//! GTK app (never anything reachable through the control socket — see
//! `App::approve_control_grant`/`deny_control_grant` in
//! `app/delegated_control.rs`) may approve or deny that request. Grants are
//! pure runtime state — never part of `Store`/`WorkspaceRecord` — scoped to
//! the exact `(workspace_id, agent_id)` pair that asked, and vanish on
//! revoke, that agent stopping, its workspace closing/unloading, or Duet
//! exiting (nothing here is persisted, so exit clears everything for free).

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
        self.grants
            .insert((request.workspace_id, request.agent_id), request.capabilities.clone());
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
        self.pending.retain(|_, request| request.workspace_id != workspace_id);
    }

    pub fn has_capability(&self, workspace_id: Uuid, agent_id: Uuid, capability: ControlCapability) -> bool {
        self.grants
            .get(&(workspace_id, agent_id))
            .is_some_and(|caps| caps.contains(&capability))
    }

    pub fn has_any_grant(&self, workspace_id: Uuid, agent_id: Uuid) -> bool {
        self.grants.contains_key(&(workspace_id, agent_id))
    }

    pub fn granted_capabilities(&self, workspace_id: Uuid, agent_id: Uuid) -> BTreeSet<ControlCapability> {
        self.grants.get(&(workspace_id, agent_id)).cloned().unwrap_or_default()
    }
}
```

Add to `src/orchestration/mod.rs`:

```rust
pub mod control_grant;
pub use control_grant::{ControlCapability, DelegatedControlService, PendingGrantRequest};
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib orchestration::control_grant`
Expected: PASS (7 tests).

- [ ] **Step 5: Commit**

```bash
git add src/orchestration/control_grant.rs src/orchestration/mod.rs
git commit -m "feat: add delegated canvas-control grant service"
```

---

### Task 2: Wire `DelegatedControlService` into `App` + revocation hooks

**Files:**
- Modify: `src/app.rs`

**Interfaces:**
- Consumes: `DelegatedControlService` (Task 1).
- Produces: `App.control: DelegatedControlService` field; `App::require_control_capability(app, requested_by, capability) -> anyhow::Result<()>`.

- [ ] **Step 1: Write a failing test for the capability gate and revocation-on-close**

Add near `require_manager`'s existing tests in `src/app.rs`'s `#[cfg(test)] mod tests`:

```rust
#[test]
fn require_control_capability_allows_the_human_operator() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    assert!(App::require_control_capability(&app, None, ControlCapability::SpawnAgents).is_ok());
}

#[test]
fn require_control_capability_denies_an_ungranted_agent() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let agent = Uuid::new_v4();
    assert!(App::require_control_capability(&app, Some(agent), ControlCapability::SpawnAgents).is_err());
}

#[test]
fn closing_a_node_revokes_that_agents_control_grant() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let agent_node = terminal_node(EnvironmentKind::LocalPty);
    let agent_id = agent_node.id;
    materialize_node(&app, agent_node, &adw::ToastOverlay::new()).unwrap();
    let workspace_id = app.borrow().workspace_id;
    {
        let mut app_mut = app.borrow_mut();
        let request_id = app_mut.control.request_grant(
            agent_id,
            workspace_id,
            [ControlCapability::MoveCanvasNodes].into_iter().collect(),
            None,
        );
        app_mut.control.approve(request_id).unwrap();
    }
    assert!(App::require_control_capability(&app, Some(agent_id), ControlCapability::MoveCanvasNodes).is_ok());

    App::close_node(&app, agent_id);

    assert!(App::require_control_capability(&app, Some(agent_id), ControlCapability::MoveCanvasNodes).is_err());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib app::tests::require_control_capability`
Expected: FAIL to compile — no `control` field, no `require_control_capability`.

- [ ] **Step 3: Implement**

In `App`'s struct body (near `pub runtime: SessionRuntime,`), add:

```rust
/// Milestone 7.6: delegated canvas-control grants for this process's
/// agents. Never persisted — see `orchestration::control_grant`'s module
/// doc.
pub control: DelegatedControlService,
```

In `App::new`'s struct literal, add `control: DelegatedControlService::new(),` alongside the other runtime-only fields (next to `bus: MessageBus::new(),`).

Add the import at the top of `src/app.rs`: `use crate::orchestration::control_grant::{ControlCapability, DelegatedControlService};` (fold into the existing `use crate::orchestration::{...}` import if one exists).

Add the gate function next to `require_manager`:

```rust
/// The Milestone 7.6 analogue of `require_manager`, for delegated
/// canvas-control actions: `None` is the trusted human operator (always
/// `Ok`); `Some(agent_id)` must hold `capability` over the active
/// workspace. Checked at the `App::` method boundary, not only in
/// `control.rs`'s CLI parsing (§7).
fn require_control_capability(
    app: &Rc<RefCell<App>>,
    requested_by: Option<Uuid>,
    capability: ControlCapability,
) -> anyhow::Result<()> {
    let Some(agent_id) = requested_by else {
        return Ok(());
    };
    let app_ref = app.borrow();
    if app_ref.control.has_capability(app_ref.workspace_id, agent_id, capability) {
        Ok(())
    } else {
        anyhow::bail!("not authorized: {agent_id} lacks delegated control capability {capability:?}")
    }
}
```

Hook revocation into the four lifecycle points research identified. In `close_node` (around line 2788), after the existing `let removed = app_mut.nodes.remove(&id);` block finishes mutating and before `drop`/returning, add:

```rust
app_mut.control.revoke(app_mut.workspace_id, id);
```

In `detach_active_workspace` (around line 776) and `teardown_active_workspace` (around line 807), each of which already tears down every node/process in the *currently active* workspace, add right after reading `app_mut.workspace_id` (or borrow it fresh):

```rust
let workspace_id = app_mut.workspace_id;
app_mut.control.clear_workspace(workspace_id);
```

placed wherever that function already has a mutable borrow of `app_mut` convenient to the existing teardown logic (match the function's existing borrow structure rather than adding a new one).

In `unload_workspace` (around line 1080), after confirming the target `id` is a *different*, dormant workspace, add:

```rust
app.borrow_mut().control.clear_workspace(id);
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib app::tests::require_control_capability app::tests::closing_a_node_revokes`
Expected: PASS (3 tests).

- [ ] **Step 5: Run the full existing app.rs test module to check for regressions**

Run: `cargo test --lib app::tests`
Expected: PASS, same count as before plus the 3 new tests.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs
git commit -m "feat: gate delegated control at the App:: boundary and revoke it on node/workspace teardown"
```

---

### Task 3: Request/approve/deny/status plumbing + the human-approval GTK dialog

**Files:**
- Modify: `src/app/files.rs` (make `window_of` `pub(super)`)
- Create: `src/app/delegated_control.rs`
- Modify: `src/app.rs` (`mod delegated_control;`)
- Modify: `src/control.rs`

**Interfaces:**
- Consumes: `App.control` (Task 2), `App::toaster`, `App::notify`, `App::agent_registry`.
- Produces: `App::request_control_grant(app, requested_by: Uuid, capabilities, reason) -> Uuid`; `App::approve_control_grant(app, request_id) -> Result<PendingGrantRequest, String>`; `App::deny_control_grant(app, request_id) -> Result<PendingGrantRequest, String>`; `App::revoke_control_grant(app, agent_id) -> bool`; `App::control_status(app, agent_id) -> BTreeSet<ControlCapability>`; `ControlRequest::ControlRequestGrant{requested_by: Uuid, capabilities: Vec<String>, reason: Option<String>}`; `ControlRequest::ControlStatus{requested_by: Option<Uuid>}`; `ControlResponse::ControlRequestPending{request_id: Uuid}`; `ControlResponse::ControlStatus{granted: Vec<String>}`.

- [ ] **Step 1: Change `window_of`'s visibility**

In `src/app/files.rs`, change:

```rust
fn window_of(widget: &impl IsA<gtk4::Widget>) -> Option<gtk4::Window> {
```

to:

```rust
pub(super) fn window_of(widget: &impl IsA<gtk4::Widget>) -> Option<gtk4::Window> {
```

- [ ] **Step 2: Write failing tests for the App-level service methods**

Add to `src/app.rs`'s test module:

```rust
#[test]
#[ignore = "needs a display"]
fn approving_a_request_grants_exactly_what_was_requested_and_denies_are_not_reachable_from_control_rs() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let agent = Uuid::new_v4();
    let request_id = app.borrow_mut().control.request_grant(
        agent,
        app.borrow().workspace_id,
        [ControlCapability::SpawnAgents].into_iter().collect(),
        Some("test".into()),
    );
    let approved = App::approve_control_grant(&app, request_id).unwrap();
    assert_eq!(approved.agent_id, agent);
    assert!(App::require_control_capability(&app, Some(agent), ControlCapability::SpawnAgents).is_ok());
    assert_eq!(
        App::control_status(&app, agent),
        [ControlCapability::SpawnAgents].into_iter().collect()
    );
}

#[test]
#[ignore = "needs a display"]
fn revoke_control_grant_takes_effect_immediately() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let agent = Uuid::new_v4();
    let request_id = app.borrow_mut().control.request_grant(
        agent,
        app.borrow().workspace_id,
        [ControlCapability::SpawnAgents].into_iter().collect(),
        None,
    );
    App::approve_control_grant(&app, request_id).unwrap();
    assert!(App::revoke_control_grant(&app, agent));
    assert!(App::require_control_capability(&app, Some(agent), ControlCapability::SpawnAgents).is_err());
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test --lib app::tests::approving_a_request app::tests::revoke_control_grant`
Expected: FAIL to compile — the `App::` methods don't exist yet.

- [ ] **Step 4: Implement `src/app/delegated_control.rs`**

```rust
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
        let request_id = app
            .borrow_mut()
            .control
            .request_grant(requested_by, workspace_id, capabilities, reason);
        App::present_control_grant_dialog(app, request_id);
        request_id
    }

    /// `duetctl control status`'s service: the capabilities currently
    /// granted to `agent_id` in the active workspace (empty if none/denied/
    /// still pending).
    pub fn control_status(app: &Rc<RefCell<App>>, agent_id: Uuid) -> BTreeSet<ControlCapability> {
        let app_ref = app.borrow();
        app_ref.control.granted_capabilities(app_ref.workspace_id, agent_id)
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

        let (non_destructive, destructive): (Vec<_>, Vec<_>) =
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
```

In `src/app.rs`, add `mod delegated_control;` next to the other `mod` declarations (line ~30).

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib app::tests::approving_a_request app::tests::revoke_control_grant`
Expected: PASS.

- [ ] **Step 6: Extend `src/control.rs`'s wire protocol and CLI**

Add two `ControlRequest` variants next to `AgentsCreate`:

```rust
/// `duet control request`: an Agent asks for delegated canvas-control
/// capabilities. Never approves anything by itself — see
/// `App::request_control_grant`; approval is only ever a GTK dialog
/// response (§6).
ControlRequestGrant {
    requested_by: Uuid,
    /// Kebab-case `ControlCapability` names (`spawn-agents`,
    /// `create-canvas-nodes`, …) — see `parse_control_capability`.
    capabilities: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
},
/// `duet control status`: capabilities currently granted to the caller
/// (or, with no acting agent, always reports every capability, since the
/// human operator is implicitly trusted — matches `require_manager`'s
/// `None` ⇒ `Ok` convention).
ControlStatus {
    #[serde(default)]
    requested_by: Option<Uuid>,
},
```

Add matching `ControlResponse` variants next to `ControlResponse::Sent`:

```rust
ControlRequestPending {
    request_id: Uuid,
},
ControlGrantStatus {
    granted: Vec<String>,
},
```

Add a parser next to `parse_provider`:

```rust
/// A `ControlCapability` as `duet control request --capability <...>`
/// accepts it — the same kebab-case the enum's `serde` rename uses, so
/// this list must track `ControlCapability`'s variants exactly.
fn parse_control_capability(name: &str) -> Result<ControlCapability, String> {
    match name {
        "spawn-agents" => Ok(ControlCapability::SpawnAgents),
        "create-canvas-nodes" => Ok(ControlCapability::CreateCanvasNodes),
        "move-canvas-nodes" => Ok(ControlCapability::MoveCanvasNodes),
        "resize-canvas-nodes" => Ok(ControlCapability::ResizeCanvasNodes),
        "configure-canvas-nodes" => Ok(ControlCapability::ConfigureCanvasNodes),
        "connect-canvas-nodes" => Ok(ControlCapability::ConnectCanvasNodes),
        "remove-canvas-nodes" => Ok(ControlCapability::RemoveCanvasNodes),
        other => Err(format!("unknown capability '{other}'")),
    }
}
```

Add argv parsing (follow the existing hand-rolled style used for `agents create`/`send` in `run_cli`): `duet control request --capability <name> [--capability <name> ...] [--reason "<text>"]` resolves the acting agent via the same `acting_agent_id()` helper every other agent-originated command already uses, builds `ControlRequestGrant`, and errors client-side if `acting_agent_id()` returns `None` (a bare human run of `duetctl` has no agent identity to request a grant *for* — this command only makes sense from inside an agent's shell). `duet control status` builds `ControlStatus { requested_by: acting_agent_id() }` with no error if `None` (reports "every capability" for the human, per the doc comment above).

Add `handle_request` arms next to `ControlRequest::AgentsCreate`:

```rust
ControlRequest::ControlRequestGrant {
    requested_by,
    capabilities,
    reason,
} => {
    let mut parsed = std::collections::BTreeSet::new();
    for name in &capabilities {
        match parse_control_capability(name) {
            Ok(cap) => {
                parsed.insert(cap);
            }
            Err(error) => return ControlResponse::Error { error },
        }
    }
    if parsed.is_empty() {
        parsed = ControlCapability::DEFAULT_SET.into_iter().collect();
    }
    let request_id = App::request_control_grant(app, requested_by, parsed, reason);
    ControlResponse::ControlRequestPending { request_id }
}
ControlRequest::ControlStatus { requested_by } => {
    let granted = match requested_by {
        None => ControlCapability::DEFAULT_SET
            .into_iter()
            .chain([ControlCapability::RemoveCanvasNodes])
            .map(|cap| format!("{cap:?}"))
            .collect(),
        Some(agent_id) => App::control_status(app, agent_id)
            .into_iter()
            .map(|cap| format!("{cap:?}"))
            .collect(),
    };
    ControlResponse::ControlGrantStatus { granted }
}
```

- [ ] **Step 7: Write a control.rs test proving an agent cannot self-approve**

```rust
#[test]
#[ignore = "needs a display"]
fn control_request_grant_never_grants_anything_by_itself() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let agent = Uuid::new_v4();
    let response = handle_request(
        &app,
        ControlRequest::ControlRequestGrant {
            requested_by: agent,
            capabilities: vec!["spawn-agents".to_string()],
            reason: None,
        },
    );
    assert!(matches!(response, ControlResponse::ControlRequestPending { .. }));
    assert!(App::require_control_capability(&app, Some(agent), ControlCapability::SpawnAgents).is_err());

    let status = handle_request(
        &app,
        ControlRequest::ControlStatus {
            requested_by: Some(agent),
        },
    );
    assert!(matches!(status, ControlResponse::ControlGrantStatus { granted } if granted.is_empty()));
}
```

- [ ] **Step 8: Run to verify it passes**

Run: `cargo test --lib control::tests`
Expected: PASS, including the new test, with zero regressions in the existing suite.

- [ ] **Step 9: Commit**

```bash
git add src/app.rs src/app/delegated_control.rs src/app/files.rs src/control.rs
git commit -m "feat: add control request/status CLI and the human-only approval dialog"
```

---

### Task 4: Authorized agent spawning with position/size/initial task

**Files:**
- Modify: `src/app.rs` (`create_session`, `create_agent`)
- Modify: `src/control.rs` (`AgentsCreate`, its `handle_request` arm, CLI parsing)

**Interfaces:**
- Consumes: `App::require_control_capability` (Task 2), existing `App::create_session`/`materialize_node`, `role::LaunchRequest.initial_prompt` (research item 6 — the same field role instructions already fold into the first launch's prompt).
- Produces: `App::create_agent(app, requested_by, name, cwd, agent, role_id, position: Option<(f64,f64)>, size: Option<(f64,f64)>, initial_prompt: Option<String>) -> anyhow::Result<Uuid>` (signature grows; update its two existing call sites — `control.rs`'s `AgentsCreate` arm and anywhere else `App::create_agent` is called).

**Design note — two independent gates, not one AND'd together:** `create_agent` keeps its existing `require_manager` check for a plain call (no `position`/`size`/`initial_prompt` — today's exact `duetctl agents create` behavior, unchanged). The moment ANY of the three new parameters is used, the call is exercising the NEW Milestone 7.6 surface, so it is gated by `require_control_capability(..., SpawnAgents)` **instead of** `require_manager` — not in addition to it. This matters: §20 says role alone must not grant control, and a human-approved `SpawnAgents` grant is a strictly more specific, more recent authorization than a standing manager role, so it must be sufficient on its own — an orchestrator agent with a grant but no manager role must still be able to use it. Requiring both would also make a grant useless for any non-manager-role agent, which the spec never asks for.

- [ ] **Step 1: Write a failing test**

```rust
#[test]
#[ignore = "needs a display"]
fn create_agent_denies_an_orchestrator_without_spawn_agents_capability() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let agent = Uuid::new_v4();
    // Using the new `position` parameter exercises the NEW SpawnAgents
    // gate, not the pre-existing require_manager one — `agent` here has
    // no grant at all (and, deliberately, no registered identity either,
    // to prove the capability check runs independently of manager status).
    let result = App::create_agent(
        &app,
        Some(agent),
        "Backend".to_string(),
        tmp.clone(),
        Agent::Shell,
        None,
        Some((100.0, 100.0)),
        None,
        None,
    );
    assert!(result.is_err());
}

#[test]
#[ignore = "needs a display"]
fn create_agent_places_an_authorized_orchestrators_agent_at_the_requested_position() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let orchestrator = Uuid::new_v4();
    let workspace_id = app.borrow().workspace_id;
    let request_id = app.borrow_mut().control.request_grant(
        orchestrator,
        workspace_id,
        [ControlCapability::SpawnAgents].into_iter().collect(),
        None,
    );
    app.borrow_mut().control.approve(request_id).unwrap();

    let id = App::create_agent(
        &app,
        Some(orchestrator),
        "Backend".to_string(),
        tmp,
        Agent::Shell,
        None,
        Some((1200.0, 500.0)),
        Some((420.0, 320.0)),
        None,
    )
    .unwrap();

    let app_ref = app.borrow();
    let entry = app_ref.nodes.get(&id).unwrap();
    assert_eq!(entry.record.position, (1200.0, 500.0));
    assert_eq!(entry.record.size, (420.0, 320.0));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib app::tests::create_agent_`
Expected: FAIL to compile — `create_agent`'s current signature has no `position`/`size`/`initial_prompt` params.

- [ ] **Step 3: Implement**

Change `App::create_agent`'s signature and body (app.rs:1853):

```rust
pub fn create_agent(
    app: &Rc<RefCell<App>>,
    requested_by: Option<Uuid>,
    name: String,
    cwd: PathBuf,
    agent: Agent,
    role_id: Option<Uuid>,
    position: Option<(f64, f64)>,
    size: Option<(f64, f64)>,
    initial_prompt: Option<String>,
) -> anyhow::Result<Uuid> {
    let using_delegated_control = position.is_some() || size.is_some() || initial_prompt.is_some();
    if using_delegated_control {
        App::require_control_capability(app, requested_by, ControlCapability::SpawnAgents)?;
    } else {
        App::require_manager(app, requested_by)?;
    }
    let name_for_toast = name.clone();
    let viewport_center_world = position.unwrap_or_else(|| App::default_spawn_position(app));
    let id = App::create_session(
        app,
        name,
        cwd,
        agent,
        None,
        role_id,
        viewport_center_world,
        &adw::ToastOverlay::new(),
    )?;
    if let Some(size) = size {
        let mut app_mut = app.borrow_mut();
        if let Some(entry) = app_mut.nodes.get_mut(&id) {
            entry.record.size = size;
            entry.widget.apply_resize(size);
        }
    }
    if let Some(prompt) = initial_prompt {
        App::queue_initial_prompt(app, id, prompt);
    }
    if let Some(agent_id) = requested_by {
        let requester = App::agent_display_name(app, agent_id);
        App::notify(app, &format!("{requester} created {name_for_toast}"));
    }
    Ok(id)
}
```

Add a small position default and the "queue an initial prompt" helper next to `create_agent`:

```rust
/// Where a spawned agent lands when the caller didn't ask for a specific
/// spot — the same "cascade so back-to-back creations don't stack exactly
/// on top of each other" default `duetctl agents create`'s `handle_request`
/// arm has always computed inline (see `src/control.rs`'s current
/// `AgentsCreate` handler); moved here so there is exactly one place that
/// owns it. Pure domain state (a node count) — no GTK/viewport coupling.
fn default_spawn_position(app: &Rc<RefCell<App>>) -> (f64, f64) {
    let count = app.borrow().nodes.len() as f64;
    (120.0 + count * 40.0, 120.0 + count * 40.0)
}

/// §9: the safest existing mechanism for "start this agent with a task" —
/// the same `LaunchRequest.initial_prompt` field role instructions and
/// handoff summaries already fold into an agent's very first prompt. No
/// new PTY-typing path.
fn queue_initial_prompt(app: &Rc<RefCell<App>>, id: Uuid, prompt: String) {
    let mut app_mut = app.borrow_mut();
    if let Some(entry) = app_mut.nodes.get_mut(&id)
        && let Some(terminal) = entry.record.kind.as_terminal_mut()
    {
        terminal.pending_initial_prompt = Some(prompt);
    }
}
```

This last step assumes `TerminalPayload` does not yet have a `pending_initial_prompt`-style field distinct from role-instruction folding — **before writing this**, read `role.rs`'s `with_role_instructions`/`LaunchRequest.initial_prompt` and `build_terminal_launch` (app.rs ~4673) end to end to find the exact existing seam where a role's instructions get folded into the first prompt, and reuse that exact seam for an orchestrator-supplied task instead of inventing a new field, per research item 6 and the Global Constraints "reuse existing services" rule. If the seam takes a `&str`/`Option<&str>` read at spawn time rather than a persisted field, thread `initial_prompt` through `create_session`'s call to `build_terminal_launch` instead of storing it on `TerminalPayload`. Adjust this step to match what you find; do not persist a prompt that was always meant to be transient.

Update every existing call site of `App::create_agent` (at minimum `control.rs`'s `AgentsCreate` arm — read it first; it currently computes its own cascade position inline right before calling `create_agent`, shown in the research as `let position = { let count = app.borrow().nodes.len() as f64; (120.0 + count * 40.0, 120.0 + count * 40.0) };`). Delete that inline computation — it is now `App::default_spawn_position`'s job — and change the call to pass `None` for `position` whenever the new `--x`/`--y` CLI flags are absent, `None, None` for `size`/`initial_prompt` when `--width`/`--height`/`--task` are absent, exactly preserving today's visual cascade behavior for the pre-existing non-spatial `duetctl agents create` (same numbers, now computed in one place instead of two).

Extend `ControlRequest::AgentsCreate` with four new `#[serde(default)]` fields:

```rust
AgentsCreate {
    name: String,
    cwd: PathBuf,
    agent: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    requested_by: Option<Uuid>,
    #[serde(default)]
    x: Option<f64>,
    #[serde(default)]
    y: Option<f64>,
    #[serde(default)]
    width: Option<f64>,
    #[serde(default)]
    height: Option<f64>,
    #[serde(default)]
    task: Option<String>,
},
```

Update its `handle_request` arm to build `position`/`size` from `(x, y)`/`(width, height)` (both present or both absent each pair — return `ControlResponse::Error` on a partial pair) and pass `task` through as `initial_prompt`.

Add CLI flags `--x`, `--y`, `--width`, `--height`, `--task` to `duetctl agents create`'s argv parser, alongside the existing `--role` flag.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib app::tests::create_agent_ control::tests::agents_create`
Expected: PASS.

- [ ] **Step 5: Run the full test suite for regressions**

Run: `cargo test --lib`
Expected: PASS, no regressions from the signature change.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs src/control.rs
git commit -m "feat: let an authorized orchestrator place a spawned agent and give it an initial task"
```

---

### Task 5: Move and resize services

**Files:**
- Modify: `src/app.rs`
- Modify: `src/control.rs`

**Interfaces:**
- Consumes: `App::require_control_capability`, existing `CanvasCommand::MoveNodes`/`ResizeNode`, `entry.record.locked`.
- Produces: `App::move_node(app, requested_by, id, position) -> Result<(), String>`; `App::resize_node(app, requested_by, id, size) -> Result<(), String>`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
#[ignore = "needs a display"]
fn move_node_respects_a_locked_node() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let mut note = note_with_markdown("a");
    note.locked = true;
    let id = note.id;
    let original = note.position;
    materialize_node(&app, note, &adw::ToastOverlay::new()).unwrap();

    let result = App::move_node(&app, None, id, (900.0, 900.0));

    assert!(result.is_err());
    assert_eq!(app.borrow().nodes.get(&id).unwrap().record.position, original);
}

#[test]
#[ignore = "needs a display"]
fn move_node_requires_move_canvas_nodes_capability_for_an_agent() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let note = note_with_markdown("a");
    let id = note.id;
    materialize_node(&app, note, &adw::ToastOverlay::new()).unwrap();
    let agent = Uuid::new_v4();

    assert!(App::move_node(&app, Some(agent), id, (10.0, 10.0)).is_err());

    let workspace_id = app.borrow().workspace_id;
    let request_id = app.borrow_mut().control.request_grant(
        agent,
        workspace_id,
        [ControlCapability::MoveCanvasNodes].into_iter().collect(),
        None,
    );
    app.borrow_mut().control.approve(request_id).unwrap();

    assert!(App::move_node(&app, Some(agent), id, (10.0, 10.0)).is_ok());
    assert_eq!(app.borrow().nodes.get(&id).unwrap().record.position, (10.0, 10.0));
}

#[test]
#[ignore = "needs a display"]
fn resize_node_pushes_an_undo_entry() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let note = note_with_markdown("a");
    let id = note.id;
    let old_size = note.size;
    materialize_node(&app, note, &adw::ToastOverlay::new()).unwrap();

    App::resize_node(&app, None, id, (500.0, 400.0)).unwrap();

    assert_eq!(app.borrow().nodes.get(&id).unwrap().record.size, (500.0, 400.0));
    assert!(matches!(
        app.borrow().undo_stack.last(),
        Some(CanvasCommand::ResizeNode { old_size: o, new_size: (500.0, 400.0), .. }) if *o == old_size
    ));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib app::tests::move_node_ app::tests::resize_node_`
Expected: FAIL to compile — no such methods yet.

- [ ] **Step 3: Implement**

```rust
/// §13/§15's "move node" service: the same mutation the GTK drag handler
/// performs (position write, widget sync, `CanvasCommand::MoveNodes`
/// undo entry, persist), reachable by stable id instead of a pointer
/// gesture, respecting `locked` exactly like the drag handler does.
pub fn move_node(
    app: &Rc<RefCell<App>>,
    requested_by: Option<Uuid>,
    id: Uuid,
    position: (f64, f64),
) -> Result<(), String> {
    App::require_control_capability(app, requested_by, ControlCapability::MoveCanvasNodes)
        .map_err(|error| error.to_string())?;
    let old_position = {
        let mut app_mut = app.borrow_mut();
        let Some(entry) = app_mut.nodes.get_mut(&id) else {
            return Err("that node no longer exists".to_string());
        };
        if entry.record.locked {
            return Err("that node is locked".to_string());
        }
        let old_position = entry.record.position;
        entry.record.position = position;
        app_mut.canvas.reposition_node(entry.widget.container(), position);
        old_position
    };
    if old_position != position {
        App::push_undo(
            app,
            CanvasCommand::MoveNodes {
                moves: vec![(id, old_position, position)],
            },
        );
    }
    if let Some(agent_id) = requested_by {
        let requester = App::agent_display_name(app, agent_id);
        let node_name = App::node_display_name(app, id);
        App::notify(app, &format!("{requester} moved {node_name}"));
    }
    let _ = app.borrow().persist();
    Ok(())
}

/// §13/§15's "resize node" service, same shape as `move_node`.
pub fn resize_node(
    app: &Rc<RefCell<App>>,
    requested_by: Option<Uuid>,
    id: Uuid,
    size: (f64, f64),
) -> Result<(), String> {
    App::require_control_capability(app, requested_by, ControlCapability::ResizeCanvasNodes)
        .map_err(|error| error.to_string())?;
    let old_size = {
        let mut app_mut = app.borrow_mut();
        let Some(entry) = app_mut.nodes.get_mut(&id) else {
            return Err("that node no longer exists".to_string());
        };
        if entry.record.locked {
            return Err("that node is locked".to_string());
        }
        let old_size = entry.record.size;
        let size = (
            size.0.max(crate::node::MIN_NODE_WIDTH),
            size.1.max(crate::node::MIN_NODE_HEIGHT),
        );
        entry.record.size = size;
        entry.widget.apply_resize(size);
        old_size
    };
    if old_size != size {
        App::push_undo(app, CanvasCommand::ResizeNode { id, old_size, new_size: size });
    }
    if let Some(agent_id) = requested_by {
        let requester = App::agent_display_name(app, agent_id);
        let node_name = App::node_display_name(app, id);
        App::notify(app, &format!("{requester} resized {node_name}"));
    }
    let _ = app.borrow().persist();
    Ok(())
}

/// A short human-readable label for activity toasts — a Terminal's/
/// Group's name, a generic kind label otherwise. Read-only, GTK-free.
fn node_display_name(app: &Rc<RefCell<App>>, id: Uuid) -> String {
    let app_ref = app.borrow();
    match app_ref.nodes.get(&id).map(|entry| &entry.record.kind) {
        Some(NodeKind::Terminal(terminal)) => terminal.name.clone(),
        Some(NodeKind::Group(group)) if !group.label.is_empty() => group.label.clone(),
        Some(kind) => kind.kind_label().to_string(),
        None => id.to_string(),
    }
}
```

Before writing `node_display_name`, confirm `NodeKind`'s existing `&'static str` label method name (model.rs:394's match, used by `as_terminal`'s neighbors) — reuse it rather than inventing `kind_label` if a differently-named equivalent already exists.

Add `ControlRequest::CanvasMove { requested_by: Option<Uuid>, id: Uuid, x: f64, y: f64 }` and `CanvasResize { requested_by: Option<Uuid>, id: Uuid, width: f64, height: f64 }`, their `handle_request` arms calling the two new services and mapping `Ok`/`Err` to `ControlResponse::Ok`/`ControlResponse::Error` (reuse whichever success-shaped `ControlResponse` variant the codebase already uses for a no-payload success, e.g. check how `NotesReplace`/`GitStage` report success before adding a new variant), and CLI parsing for `duet canvas move <id> --x <f64> --y <f64>` / `duet canvas resize <id> --width <f64> --height <f64>`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib app::tests::move_node_ app::tests::resize_node_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/control.rs
git commit -m "feat: add authorized move/resize canvas services and duetctl canvas move/resize"
```

---

### Task 6: Generic canvas node creation (Note/Text/FileTree/Group/Drawing/Portal)

**Files:**
- Modify: `src/app.rs`
- Modify: `src/control.rs`

**Interfaces:**
- Consumes: `App::add_simple_node` (existing, private — reuse, do not duplicate), `NodeKind` variants, `App::require_control_capability`.
- Produces: `App::create_canvas_node(app, requested_by, kind: NodeKind, position: (f64,f64), size: Option<(f64,f64)>) -> Result<Uuid, String>`.

- [ ] **Step 1: Write a failing test**

```rust
#[test]
#[ignore = "needs a display"]
fn create_canvas_node_requires_create_canvas_nodes_capability() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let agent = Uuid::new_v4();
    let kind = NodeKind::Group(GroupPayload::default());

    assert!(App::create_canvas_node(&app, Some(agent), kind.clone(), (0.0, 0.0), None).is_err());

    let workspace_id = app.borrow().workspace_id;
    let request_id = app.borrow_mut().control.request_grant(
        agent,
        workspace_id,
        [ControlCapability::CreateCanvasNodes].into_iter().collect(),
        None,
    );
    app.borrow_mut().control.approve(request_id).unwrap();

    let id = App::create_canvas_node(&app, Some(agent), kind, (100.0, 200.0), Some((300.0, 150.0))).unwrap();
    let entry = app.borrow().nodes.get(&id).cloned();
    assert_eq!(entry.unwrap().record.position, (100.0, 200.0));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib app::tests::create_canvas_node_`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
/// §11's orchestrator-facing node creation: the exact same
/// `add_simple_node` the GUI's Note/Group/Drawing buttons already use,
/// gated by `CreateCanvasNodes` — never a second, parallel creation path.
/// `size` falls back to `kind`'s usual default when omitted.
pub fn create_canvas_node(
    app: &Rc<RefCell<App>>,
    requested_by: Option<Uuid>,
    kind: NodeKind,
    position: (f64, f64),
    size: Option<(f64, f64)>,
) -> Result<Uuid, String> {
    App::require_control_capability(app, requested_by, ControlCapability::CreateCanvasNodes)
        .map_err(|error| error.to_string())?;
    if matches!(kind, NodeKind::Terminal(_)) {
        return Err("use control request/agents create for Terminal nodes".to_string());
    }
    let size = size.unwrap_or_else(|| App::default_size_for(&kind));
    let id = App::add_simple_node(app, kind, position, size);
    if let Some(agent_id) = requested_by {
        let requester = App::agent_display_name(app, agent_id);
        App::notify(app, &format!("{requester} created {}", App::node_display_name(app, id)));
    }
    Ok(id)
}

/// The same per-kind default size `create_note`/`create_group`/
/// `create_drawing` each hard-code today — centralized here so the
/// orchestrator path and a future caller agree on one table instead of
/// guessing a size.
fn default_size_for(kind: &NodeKind) -> (f64, f64) {
    match kind {
        NodeKind::Note(_) | NodeKind::Text(_) => (220.0, 160.0),
        NodeKind::Group(_) => (720.0, 460.0),
        NodeKind::Drawing(_) => App::drawing_default_size(),
        NodeKind::FileTree(_) => (320.0, 420.0),
        NodeKind::Portal(_) => (640.0, 480.0),
        _ => (320.0, 240.0),
    }
}
```

Before finalizing `default_size_for`'s `Drawing` arm, read `create_drawing`'s actual hard-coded size (app.rs ~2028) and either inline the same two numbers or call whatever existing constant/function it already uses — do not guess at a different default than the GUI's.

Add `ControlRequest::CanvasCreate { requested_by: Option<Uuid>, kind: String, name: Option<String>, x: f64, y: f64, width: Option<f64>, height: Option<f64> }`, where `kind` is one of `"note" | "text" | "filetree" | "group" | "drawing" | "portal"`. Its `handle_request` arm builds the right `NodeKind` (reuse `PortalPayload::new(name, "")` for `"portal"`, `GroupPayload { label: name.unwrap_or_default(), ..Default::default() }` for `"group"`, `FileTreePayload::default()` for `"filetree"` — read `model.rs`'s exact `Default` impls for each before writing the literal, per Task 3-style payload construction already verified for `Note`/`Text` in `create_note`/`create_text_node`) and calls `App::create_canvas_node`. Add CLI parsing for `duet canvas create --kind <kind> [--name <text>] --x <f64> --y <f64> [--width <f64>] [--height <f64>]`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib app::tests::create_canvas_node_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/control.rs
git commit -m "feat: add authorized generic canvas node creation and duetctl canvas create"
```

---

### Task 7: Configure (rename / recolor) a node

**Files:**
- Modify: `src/app.rs`
- Modify: `src/control.rs`

**Interfaces:**
- Consumes: `App::require_control_capability`, `App::update_group` (existing, reuse for Group), `CanvasCommand::SetProperties`.
- Produces: `App::configure_node(app, requested_by, id, name: Option<String>, color: Option<String>) -> Result<(), String>`.

- [ ] **Step 1: Write a failing test**

```rust
#[test]
#[ignore = "needs a display"]
fn configure_node_renames_a_terminal_when_authorized() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let node = terminal_node(EnvironmentKind::LocalPty);
    let id = node.id;
    materialize_node(&app, node, &adw::ToastOverlay::new()).unwrap();
    let agent = Uuid::new_v4();
    let workspace_id = app.borrow().workspace_id;
    let request_id = app.borrow_mut().control.request_grant(
        agent,
        workspace_id,
        [ControlCapability::ConfigureCanvasNodes].into_iter().collect(),
        None,
    );
    app.borrow_mut().control.approve(request_id).unwrap();

    App::configure_node(&app, Some(agent), id, Some("Backend".to_string()), None).unwrap();

    assert_eq!(
        app.borrow().nodes.get(&id).unwrap().record.as_terminal().unwrap().name,
        "Backend"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib app::tests::configure_node_`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
/// §11's "rename/configure node": a Terminal's display name or a Group's
/// label/color, as one undoable `SetProperties` edit. Deliberately narrow
/// — this is not a general property-bag setter, just the two fields the
/// spec calls out (§1's "rename/configure newly created Agents",
/// §10/§11's "rename/configure node").
pub fn configure_node(
    app: &Rc<RefCell<App>>,
    requested_by: Option<Uuid>,
    id: Uuid,
    name: Option<String>,
    color: Option<String>,
) -> Result<(), String> {
    App::require_control_capability(app, requested_by, ControlCapability::ConfigureCanvasNodes)
        .map_err(|error| error.to_string())?;
    let kind = app
        .borrow()
        .nodes
        .get(&id)
        .map(|entry| entry.record.kind.clone())
        .ok_or_else(|| "that node no longer exists".to_string())?;
    match kind {
        NodeKind::Group(_) => App::update_group(app, id, name.as_deref(), color.as_deref())?,
        NodeKind::Terminal(_) => {
            let Some(name) = name else {
                return Err("a Terminal node only has a name to configure".to_string());
            };
            let (before, after) = {
                let mut app_mut = app.borrow_mut();
                let entry = app_mut.nodes.get_mut(&id).ok_or("that node no longer exists")?;
                let before = entry.record.clone();
                let Some(terminal) = entry.record.kind.as_terminal_mut() else {
                    return Err("that node is not a Terminal".to_string());
                };
                terminal.name = name.trim().to_string();
                (before, entry.record.clone())
            };
            App::push_undo(app, CanvasCommand::SetProperties { before, after });
        }
        _ => return Err("that node kind has nothing to configure yet".to_string()),
    }
    if let Some(agent_id) = requested_by {
        let requester = App::agent_display_name(app, agent_id);
        App::notify(app, &format!("{requester} configured {}", App::node_display_name(app, id)));
    }
    let _ = app.borrow().persist();
    Ok(())
}
```

Before finalizing, confirm `CanvasCommand::SetProperties`'s exact field names (`before`/`after`) by reading its definition next to `MoveNodes`/`ResizeNode` in the same enum.

Add `ControlRequest::CanvasConfigure { requested_by: Option<Uuid>, id: Uuid, name: Option<String>, color: Option<String> }`, its `handle_request` arm, and `duet canvas configure <id> [--name <text>] [--color <text>]` CLI parsing.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib app::tests::configure_node_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/control.rs
git commit -m "feat: add authorized node rename/configure service and duetctl canvas configure"
```

---

### Task 8: Authorized connect (extend `create_edge`)

**Files:**
- Modify: `src/app.rs`
- Modify: `src/control.rs`

**Interfaces:**
- Consumes: `App::create_edge` (existing — extend its signature, update every call site), `App::require_control_capability`.
- Produces: `App::create_edge(app, requested_by: Option<Uuid>, source: Uuid, target: Uuid) -> Result<bool, String>` (signature change — was `-> bool` with no `requested_by`; this is the one pre-existing public function this plan changes the signature of besides `create_agent`, so grep every call site before editing).

- [ ] **Step 1: Write a failing test**

```rust
#[test]
#[ignore = "needs a display"]
fn create_edge_requires_connect_canvas_nodes_capability_for_an_agent() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let a = terminal_node(EnvironmentKind::LocalPty);
    let b = terminal_node(EnvironmentKind::LocalPty);
    let (id_a, id_b) = (a.id, b.id);
    materialize_node(&app, a, &adw::ToastOverlay::new()).unwrap();
    materialize_node(&app, b, &adw::ToastOverlay::new()).unwrap();
    let agent = Uuid::new_v4();

    assert!(App::create_edge(&app, Some(agent), id_a, id_b).unwrap_err().contains("not authorized"));

    let workspace_id = app.borrow().workspace_id;
    let request_id = app.borrow_mut().control.request_grant(
        agent,
        workspace_id,
        [ControlCapability::ConnectCanvasNodes].into_iter().collect(),
        None,
    );
    app.borrow_mut().control.approve(request_id).unwrap();

    assert!(App::create_edge(&app, Some(agent), id_a, id_b).unwrap());
}

#[test]
#[ignore = "needs a display"]
fn create_edge_still_works_unauthenticated_for_the_human_operator() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let a = terminal_node(EnvironmentKind::LocalPty);
    let b = terminal_node(EnvironmentKind::LocalPty);
    let (id_a, id_b) = (a.id, b.id);
    materialize_node(&app, a, &adw::ToastOverlay::new()).unwrap();
    materialize_node(&app, b, &adw::ToastOverlay::new()).unwrap();

    assert!(App::create_edge(&app, None, id_a, id_b).unwrap());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib app::tests::create_edge_`
Expected: FAIL to compile — `create_edge`'s current signature takes no `requested_by` and returns `bool`, not `Result<bool, String>`.

- [ ] **Step 3: Implement**

Change `App::create_edge`'s signature (app.rs:2177) to:

```rust
pub fn create_edge(
    app: &Rc<RefCell<App>>,
    requested_by: Option<Uuid>,
    source: Uuid,
    target: Uuid,
) -> Result<bool, String> {
    App::require_control_capability(app, requested_by, ControlCapability::ConnectCanvasNodes)
        .map_err(|error| error.to_string())?;
    let mut app = app.borrow_mut();
    // ... existing body unchanged, with its final `bool` wrapped in `Ok(...)` ...
```

Wrap every existing `return false`/trailing `false`/`true` in the function body with `Ok(...)`. Update every call site (grep `App::create_edge(` across `src/`) to pass `None` for the GUI's existing click-to-connect flow (human-initiated, so `None` is correct and preserves today's behavior) and to handle the new `Result`. The existing `create_edge_defaults_capabilities_by_the_endpoints_kinds` test (app.rs ~5429) must also be updated to the new call shape (`App::create_edge(&app, None, a, b).unwrap()` instead of `assert!(App::create_edge(&app, a, b))`), since it's an existing call site.

If `requested_by.is_some()` and the call succeeds, add an activity toast:

```rust
    if requested_by.is_some() && result {
        // (inside the Ok(true) path, after re-borrowing `app` outside the
        // `app.borrow_mut()` block above)
    }
```

Place this using the same `App::agent_display_name`/`App::node_display_name`/`App::notify` pattern as the other services, after the mutable borrow of `app` has been dropped.

Add `ControlRequest::CanvasConnect { requested_by: Option<Uuid>, source: Uuid, target: Uuid }`, its `handle_request` arm, and `duet canvas connect <source-id> <target-id>` CLI parsing. Note §16: this deliberately does **not** take an explicit capability list — it inherits whatever default capabilities `create_edge`'s existing kind-pair table assigns, which is already least-privilege by construction (research item 8); do not add a capability-selection flag to this command.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib app::tests::create_edge_`
Expected: PASS.

- [ ] **Step 5: Run the full suite — this task touches a widely-called existing function**

Run: `cargo test --lib`
Expected: PASS. Pay particular attention to any test or call site this plan's research did not enumerate; `grep -rn "App::create_edge(" src/` before considering this task done.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs src/control.rs
git commit -m "feat: gate node connection behind ConnectCanvasNodes for agent-originated requests"
```

---

### Task 9: Removing non-agent canvas nodes (opt-in `RemoveCanvasNodes`)

**Files:**
- Modify: `src/app.rs`
- Modify: `src/control.rs`

**Interfaces:**
- Consumes: `App::require_control_capability`, `CanvasCommand::RemoveNodes`.
- Produces: `App::remove_canvas_node(app, requested_by, id) -> Result<(), String>` — refuses Terminal nodes outright (agent termination stays on `remove_agent`/`require_manager` only, per Global Constraints).

- [ ] **Step 1: Write failing tests**

```rust
#[test]
#[ignore = "needs a display"]
fn remove_canvas_node_refuses_terminal_nodes() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let node = terminal_node(EnvironmentKind::LocalPty);
    let id = node.id;
    materialize_node(&app, node, &adw::ToastOverlay::new()).unwrap();

    assert!(App::remove_canvas_node(&app, None, id).is_err());
    assert!(app.borrow().nodes.contains_key(&id));
}

#[test]
#[ignore = "needs a display"]
fn remove_canvas_node_requires_the_destructive_capability_explicitly() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let note = note_with_markdown("a");
    let id = note.id;
    materialize_node(&app, note, &adw::ToastOverlay::new()).unwrap();
    let agent = Uuid::new_v4();
    let workspace_id = app.borrow().workspace_id;

    // The default grant (everything in DEFAULT_SET) must NOT cover this.
    let request_id = app.borrow_mut().control.request_grant(
        agent,
        workspace_id,
        ControlCapability::DEFAULT_SET.into_iter().collect(),
        None,
    );
    app.borrow_mut().control.approve(request_id).unwrap();
    assert!(App::remove_canvas_node(&app, Some(agent), id).is_err());

    let request_id = app.borrow_mut().control.request_grant(
        agent,
        workspace_id,
        [ControlCapability::RemoveCanvasNodes].into_iter().collect(),
        None,
    );
    app.borrow_mut().control.approve(request_id).unwrap();
    assert!(App::remove_canvas_node(&app, Some(agent), id).is_ok());
    assert!(!app.borrow().nodes.contains_key(&id));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib app::tests::remove_canvas_node_`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
/// §2/§17's opt-in destructive capability: deletes a non-agent canvas
/// node. Never covers Terminal nodes — an Agent is only ever removed
/// through the existing `require_manager`-gated `remove_agent` (killing a
/// process is a different risk class than deleting a Note/Group; §2's
/// "must not automatically grant... terminating existing user Agents").
/// `RemoveCanvasNodes` is never in `ControlCapability::DEFAULT_SET`, so
/// this always requires its own explicit grant.
pub fn remove_canvas_node(app: &Rc<RefCell<App>>, requested_by: Option<Uuid>, id: Uuid) -> Result<(), String> {
    App::require_control_capability(app, requested_by, ControlCapability::RemoveCanvasNodes)
        .map_err(|error| error.to_string())?;
    let removed = {
        let mut app_mut = app.borrow_mut();
        let Some(entry) = app_mut.nodes.get(&id) else {
            return Err("that node no longer exists".to_string());
        };
        if entry.record.as_terminal().is_some() {
            return Err("Terminal nodes cannot be removed this way — use agent removal".to_string());
        }
        let canvas = app_mut.canvas.clone();
        let removed = app_mut.nodes.remove(&id).expect("checked above");
        canvas.remove_node(removed.widget.container());
        app_mut.edges.retain(|edge| edge.source != id && edge.target != id);
        removed.record
    };
    App::push_undo(
        app,
        CanvasCommand::RemoveNodes {
            nodes: vec![removed],
            edges: Vec::new(),
        },
    );
    if let Some(agent_id) = requested_by {
        let requester = App::agent_display_name(app, agent_id);
        App::notify(app, &format!("{requester} removed a node"));
    }
    let _ = app.borrow().persist();
    Ok(())
}
```

Before finalizing, read `close_node`'s existing teardown (app.rs ~2788) and `request_delete`'s multi-node deletion path (app.rs ~3040) to confirm there isn't already a reusable non-terminal removal helper you'd otherwise duplicate — if one exists, call it instead of hand-rolling the block above.

Add `ControlRequest::CanvasRemove { requested_by: Option<Uuid>, id: Uuid }`, its `handle_request` arm, and `duet canvas remove <id>` CLI parsing. Add `--capability remove-nodes` as a valid (but never implied) value to `parse_control_capability`/`duet control request`'s `--capability` flag (already covered by Task 1's enum and Task 3's parser — this task just confirms the CLI help text/docs mention it is opt-in-only).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib app::tests::remove_canvas_node_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/control.rs
git commit -m "feat: add opt-in RemoveCanvasNodes service and duetctl canvas remove"
```

---

### Task 10: Read-only canvas listing/inspection

**Files:**
- Modify: `src/app.rs`
- Modify: `src/control.rs`

**Interfaces:**
- Produces: `App::list_canvas_nodes(app) -> Vec<CanvasNodeSummary>`; `App::inspect_canvas_node(app, id) -> Option<CanvasNodeSummary>`; `CanvasNodeSummary { id, kind: String, name: Option<String>, position: (f64,f64), size: (f64,f64), locked: bool, z_order: i64 }` (a new, small serde struct — this is the one new public data type besides Task 1's).

Not capability-gated — mirrors `AgentsList`/`ConnectionsList`'s existing precedent of being plain informational reads available to any agent, needed to make spatial decisions (§13: "the canvas is structured data... provide structured geometry").

- [ ] **Step 1: Write a failing test**

```rust
#[test]
#[ignore = "needs a display"]
fn list_canvas_nodes_reports_every_live_node() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let note = note_with_markdown("a");
    let id = note.id;
    materialize_node(&app, note, &adw::ToastOverlay::new()).unwrap();

    let nodes = App::list_canvas_nodes(&app);
    assert!(nodes.iter().any(|n| n.id == id && n.kind == "Note"));

    let inspected = App::inspect_canvas_node(&app, id).unwrap();
    assert_eq!(inspected.id, id);
    assert!(App::inspect_canvas_node(&app, Uuid::new_v4()).is_none());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib app::tests::list_canvas_nodes_`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
/// §10/§13's read model for an orchestrator's spatial reasoning: enough
/// structured geometry to place/avoid-colliding-with other nodes, never
/// a screenshot (§13's explicit "do NOT give agents screenshots").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasNodeSummary {
    pub id: Uuid,
    pub kind: String,
    pub name: Option<String>,
    pub position: (f64, f64),
    pub size: (f64, f64),
    pub locked: bool,
    pub z_order: i64,
}

impl App {
    pub fn list_canvas_nodes(app: &Rc<RefCell<App>>) -> Vec<CanvasNodeSummary> {
        app.borrow()
            .nodes
            .values()
            .map(|entry| App::summarize_node(&entry.record))
            .collect()
    }

    pub fn inspect_canvas_node(app: &Rc<RefCell<App>>, id: Uuid) -> Option<CanvasNodeSummary> {
        app.borrow().nodes.get(&id).map(|entry| App::summarize_node(&entry.record))
    }

    fn summarize_node(record: &NodeRecord) -> CanvasNodeSummary {
        let name = match &record.kind {
            NodeKind::Terminal(terminal) => Some(terminal.name.clone()),
            NodeKind::Group(group) if !group.label.is_empty() => Some(group.label.clone()),
            NodeKind::Portal(portal) => Some(portal.name.clone()),
            _ => None,
        };
        CanvasNodeSummary {
            id: record.id,
            kind: record.kind.kind_label().to_string(),
            name,
            position: record.position,
            size: record.size,
            locked: record.locked,
            z_order: record.z_order,
        }
    }
}
```

Reuse `NodeKind`'s existing `&'static str` kind-label method (model.rs:394's match — confirm its real name; this plan calls it `kind_label` provisionally) instead of inventing a second one.

Add `ControlRequest::CanvasList` and `CanvasInspect { id: Uuid }`, `ControlResponse::CanvasNodes { nodes: Vec<CanvasNodeSummary> }` and `CanvasNode { node: CanvasNodeSummary }` (or `Error` if not found), their `handle_request` arms, and `duet canvas list --json` / `duet canvas inspect <id> --json` CLI parsing (print pretty JSON via `serde_json::to_string_pretty`, matching however other `--json`-flagged commands in this CLI already format output — check `drawing read`/`resource inspect` for the exact convention before adding a new one).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib app::tests::list_canvas_nodes_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/control.rs
git commit -m "feat: add read-only canvas list/inspect service and duetctl canvas list/inspect"
```

---

### Task 11: `find-space` positioning helper

**Files:**
- Modify: `src/app.rs`
- Modify: `src/control.rs`

**Interfaces:**
- Produces: `App::find_canvas_space(app, width: f64, height: f64) -> (f64, f64)`.

- [ ] **Step 1: Write a failing test**

```rust
#[test]
#[ignore = "needs a display"]
fn find_canvas_space_avoids_an_existing_nodes_bounding_box() {
    if gtk4::init().is_err() {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("duet-test-{}", Uuid::new_v4()));
    let app = App::new(AccountStore::new(tmp.join("accounts")), tmp.join("store.json"));
    let mut existing = note_with_markdown("a");
    existing.position = (0.0, 0.0);
    existing.size = (300.0, 200.0);
    materialize_node(&app, existing, &adw::ToastOverlay::new()).unwrap();

    let (x, y) = App::find_canvas_space(&app, 200.0, 150.0);

    let overlaps = x < 300.0 && x + 200.0 > 0.0 && y < 200.0 && y + 150.0 > 0.0;
    assert!(!overlaps, "found space ({x}, {y}) overlaps the existing node");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib app::tests::find_canvas_space_`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
/// §13's optional-but-simple helper: the first position (scanning a grid
/// left-to-right, top-to-bottom, starting at the origin) whose
/// `width`x`height` box doesn't overlap any existing node's bounding box.
/// A plain O(nodes × candidates) scan — fine at canvas-sized node counts;
/// this is deliberately not spatial-indexed.
pub fn find_canvas_space(app: &Rc<RefCell<App>>, width: f64, height: f64) -> (f64, f64) {
    const STEP: f64 = 40.0;
    const MAX_COLUMNS: i64 = 60;
    let boxes: Vec<((f64, f64), (f64, f64))> = app
        .borrow()
        .nodes
        .values()
        .map(|entry| (entry.record.position, entry.record.size))
        .collect();
    let overlaps = |x: f64, y: f64| {
        boxes.iter().any(|&((bx, by), (bw, bh))| x < bx + bw && x + width > bx && y < by + bh && y + height > by)
    };
    let mut row = 0i64;
    loop {
        for col in 0..MAX_COLUMNS {
            let x = col as f64 * STEP;
            let y = row as f64 * STEP;
            if !overlaps(x, y) {
                return (x, y);
            }
        }
        row += 1;
    }
}
```

Add `ControlRequest::CanvasFindSpace { width: f64, height: f64 }`, its `handle_request` arm (`ControlResponse::CanvasPosition { x: f64, y: f64 }`), and `duet canvas find-space --width <f64> --height <f64>` CLI parsing.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib app::tests::find_canvas_space_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/control.rs
git commit -m "feat: add duetctl canvas find-space positioning helper"
```

---

### Task 12: Revocation UI (per-agent indicator + revoke action)

**Files:**
- Modify: `src/app.rs` (`node_menu_items`)

**Interfaces:**
- Consumes: `App::revoke_control_grant` (Task 3), `app.control.has_any_grant` (Task 1), `separator()` (existing, app.rs:3571).

- [ ] **Step 1: Manual verification plan (GTK UI — no unit test possible below this layer)**

Document what you will check interactively after implementing (per CLAUDE.md "prefer testing below the GTK layer... use GTK integration tests only when the behavior genuinely requires GTK" — a menu item's presence is such a case):
1. Approve a test grant for a Terminal node's agent (via a `duetctl control request`/dialog "Allow" round trip, or directly in a debug build by calling `app.control.request_grant`+`approve` from a scratch test harness).
2. Open that Terminal node's card menu — confirm a "Revoke canvas control" row appears, absent for an agent with no grant.
3. Click it — confirm a toast appears and a subsequent `duetctl control status` for that agent reports no capabilities.

- [ ] **Step 2: Implement**

In `node_menu_items`'s `NodeKind::Terminal(_) => { ... }` branch (app.rs ~3720), after the existing items are built but before the branch returns `items`, add:

```rust
let workspace_id = app.borrow().workspace_id;
if app.borrow().control.has_any_grant(workspace_id, id) {
    items.push(separator());
    let app_c = Rc::clone(app);
    items.push((
        "! Revoke canvas control".to_string(),
        Box::new(move || {
            App::revoke_control_grant(&app_c, id);
        }),
    ));
}
```

The existing `"! "`-prefix convention (seen elsewhere in `popup_menu`'s own parsing — `text.strip_prefix("! ")`) already renders a menu row as destructive-styled without any new popover machinery; confirm this by reading `popup_menu`'s handling of that prefix (app.rs ~3597) before relying on it.

- [ ] **Step 3: Build and manually verify per Step 1's checklist**

Run: `cargo build` then launch the app (`cargo run`), and walk through Step 1's three checks.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "feat: show a revoke action on a Terminal node once its agent holds canvas control"
```

---

### Task 13: Skill update — teach the delegated-control workflow

**Files:**
- Modify: `src/orchestration/skill.rs`

**Interfaces:**
- Consumes: none (pure doc content).
- Produces: an extended `SKILL_MD` constant; new tests asserting the new substrings; every existing asserted substring (listed in `install_writes_a_skill_md_with_frontmatter_and_the_whoami_pointer`) stays intact.

- [ ] **Step 1: Write a failing test for the new content**

Add to `src/orchestration/skill.rs`'s test module:

```rust
#[test]
fn install_writes_the_delegated_control_workflow() {
    let tmp = tempdir().unwrap();
    install(tmp.path()).unwrap();
    let content = std::fs::read_to_string(tmp.path().join("skills").join(SKILL_NAME).join("SKILL.md")).unwrap();
    assert!(content.contains("duetctl control request"));
    assert!(content.contains("duetctl control status"));
    assert!(content.contains("duetctl canvas list"));
    assert!(content.contains("duetctl canvas create"));
    assert!(content.contains("duetctl canvas move"));
    assert!(content.contains("duetctl canvas connect"));
    assert!(content.contains("wait for approval"));
    assert!(content.contains("locked"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib orchestration::skill::tests::install_writes_the_delegated_control_workflow`
Expected: FAIL — content not present yet.

- [ ] **Step 3: Extend `SKILL_MD`**

Append a new section to the `SKILL_MD` constant (after its existing content, before the closing `"#`), following §19's exact workflow:

```markdown

## Organizing or expanding a workspace

If asked to organize or expand this Duet workspace — set up a team of
agents, create supporting notes/file trees, arrange the canvas — you do
NOT have that authority by default, even as a manager/Lead role:

1. Inspect the current canvas first: `duetctl canvas list --json`.
2. Decide what would actually help; prefer reusing an existing agent or
   node over creating a duplicate.
3. Request exactly the capabilities the task needs, with a short reason:
   `duetctl control request --capability spawn-agents --capability
   create-canvas-nodes --capability move-canvas-nodes --reason "Set up a
   frontend/backend/reviewer team for this task"`.
4. Wait for approval — this requires a human clicking "Allow" in Duet's
   own dialog; nothing you do grants it. Poll `duetctl control status`
   until the capabilities you need appear, or ask the user directly.
5. Once granted, use `duetctl agents create` (with `--x/--y/--width
   /--height/--task` to place and task a new agent), `duetctl canvas
   create/move/resize/configure/connect`, and `duetctl canvas find-space`
   to lay things out, by stable node id (`duetctl canvas list --json`).
6. If a request is denied, or a specific node is locked, or an action
   fails as "not authorized", stop and explain — never try another path
   to the same end.
7. Deleting a canvas item needs a separate `remove-canvas-nodes`
   capability, never implied by the capabilities above.
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib orchestration::skill::tests`
Expected: PASS — including every previously-existing assertion.

- [ ] **Step 5: Commit**

```bash
git add src/orchestration/skill.rs
git commit -m "docs: teach the duet skill the delegated canvas-control workflow"
```

---

### Task 14: Final verification sweep

**Files:** none (verification only).

- [ ] **Step 1:** `cargo fmt`
- [ ] **Step 2:** `cargo check`
- [ ] **Step 3:** `cargo test --lib` (full suite)
- [ ] **Step 4:** `cargo clippy --all-targets -- -D warnings` if clippy is already part of this repo's normal workflow (check `Cargo.toml`/CI config for whether clippy is already enforced; if not already a project convention, run it informationally but don't block on it).
- [ ] **Step 5:** `git diff --stat` against the branch's base — scan for any unrelated edits.
- [ ] **Step 6:** Manually launch the app (`cargo run`) and walk the end-to-end flow once: have a Shell-provider test agent run `duetctl control request --capability spawn-agents --capability create-canvas-nodes --reason "test"`, approve it in the dialog, then run `duetctl agents create --name Tester --cwd . --agent shell --x 1000 --y 1000`, confirm the new card appears at that position, then revoke via the Terminal node's menu and confirm a further `duetctl canvas create ...` from that same agent is refused.

---

## Self-Review Notes

- **Spec coverage:** §1 (Task 4, 6), §2 (Task 1), §3 (Task 1/2 — in-memory-only, revoked on close/teardown/unload), §4 (Task 3), §5 (Task 3's dialog), §6 (Task 3's structural guarantee + test), §7 (Task 2's `require_control_capability` at every service boundary), §8 (Task 4 reuses `create_session`), §9 (Task 4's `initial_prompt` seam — flagged for the implementer to locate the exact existing field rather than guessing), §10 (Task 10), §11 (Task 6), §12 (Group remains visual-only — no code changes needed, nothing in this plan adds parent/child membership), §13 (Task 10 + 11), §14 (explicitly skipped, Global Constraints), §15 (Task 5's locked-node checks, `CanvasCommand` reuse), §16 (Task 8's reuse of `create_edge`'s existing defaulting table, no capability-assignment flag added), §17 (Task 3's revoke method + Task 12's UI), §18 (toast-per-completed-action folded into Tasks 4-10, no toast on intermediate drag-equivalent ticks since there are none — every service call here is already one atomic completed action), §19 (Task 13), §20 (Global Constraints note — "Lead" already exists, role alone never grants control, no code change required beyond documentation).
- **Placeholder scan:** no TBD/"add later"/paraphrased-test steps remain; every code block is real Rust against verified signatures. Three places deliberately tell the implementer to verify one exact detail against the live source before finalizing a literal (Task 4's `initial_prompt` seam, Task 6's `Drawing` default size, Task 10's `kind_label` method name) — these are flagged, not guessed, because the research pass that grounded this plan did not pin those three specifics byte-for-byte; CLAUDE.md's "read it yourself before changing it" rule applies most directly to exactly these three spots.
- **Type consistency:** `ControlCapability`, `DelegatedControlService`, `PendingGrantRequest` (Task 1) are used with identical names/fields through Tasks 2-13. `App::create_edge`'s and `App::create_agent`'s signature changes are each called out explicitly with a "grep every call site" instruction, since both are pre-existing public functions this plan changes rather than new ones.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-10-13-orchestrator-delegated-canvas-control.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
