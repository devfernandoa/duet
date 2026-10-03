//! Universal resource addressing (Milestone 5): parses `@name` / `@kind:name`
//! references the way a human or an LLM types them in a prompt, and resolves
//! them to a stable Duet identity through the same node/edge data every
//! other orchestration service already reads — never through a GTK widget
//! tree. See CLAUDE.md's agent architecture section and `.claude/steps.md`'s
//! Milestone 5 ("Universal Resource Addressing and Skill Integration").
//!
//! Resolution answers "what does this reference point to"; it says nothing
//! about whether the caller may act on it — `permissions::authorize` (via
//! `notes::authorize_note` and friends) still gates every actual read/write,
//! the same way knowing a file exists is different from being allowed to
//! open it. See section 8: "resolution and access are different."
//!
//! Only `Agent` and `Note` are implemented today — the two resource kinds
//! that already have an identity and a service behind them. `ResourceKind`
//! is deliberately a plain enum with one `candidates_for_kind` match inside
//! this module, not a `dyn` provider registry: with two variants, a trait
//! object registry would be the "empty abstraction for naming symmetry"
//! CLAUDE.md warns against. Adding `File`/`Portal`/`Floor`/`Workspace` later
//! means adding a variant and one match arm here, nowhere else — the one
//! place this crate is allowed to know about every resource kind at once.

use super::identity::agent_identities;
use super::notes::note_title;
use crate::model::{EdgeRecord, FloorRef, NodeRecord};
use crate::role::Role;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A kind of addressable Duet resource. See this module's doc comment for
/// why this is a closed enum rather than a provider registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Agent,
    Note,
}

impl ResourceKind {
    /// Parses the qualifier before the `:` in `@kind:name` — case-insensitive
    /// since this is typed by humans and LLMs, not machine-generated.
    pub fn parse(raw: &str) -> Option<ResourceKind> {
        match raw.to_ascii_lowercase().as_str() {
            "agent" => Some(ResourceKind::Agent),
            "note" => Some(ResourceKind::Note),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ResourceKind::Agent => "agent",
            ResourceKind::Note => "note",
        }
    }
}

/// A parsed `@name` or `@kind:name` reference (section 2). Carries no
/// resolution state of its own — [`resolve`] is the only thing that turns
/// one of these into an actual resource.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRef {
    pub kind: Option<ResourceKind>,
    pub name: String,
}

impl ResourceRef {
    /// Parses `@name` or `@kind:name`. The leading `@` is required — it's
    /// the explicit, deterministic signal section 10 asks for; plain words
    /// are never upgraded into a reference by this parser.
    pub fn parse(raw: &str) -> Result<ResourceRef, String> {
        let body = raw.strip_prefix('@').ok_or_else(|| {
            format!("'{raw}' is not a resource reference (expected @name or @kind:name)")
        })?;
        if body.is_empty() {
            return Err("a resource reference needs a name after '@'".to_string());
        }
        match body.split_once(':') {
            Some((kind, name)) => {
                let kind = ResourceKind::parse(kind).ok_or_else(|| {
                    format!("unknown resource kind '{kind}' (expected agent or note)")
                })?;
                if name.is_empty() {
                    return Err(format!("'@{}:' needs a name after the colon", kind.label()));
                }
                Ok(ResourceRef {
                    kind: Some(kind),
                    name: name.to_string(),
                })
            }
            None => Ok(ResourceRef {
                kind: None,
                name: body.to_string(),
            }),
        }
    }
}

/// One resource a reference resolved to — identity plus just enough context
/// (workspace, floor) to tell two same-named resources apart (section 5).
/// Never a GTK widget; built purely from `NodeRecord`/`EdgeRecord` data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedResource {
    pub kind: ResourceKind,
    pub id: Uuid,
    pub name: String,
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub floor: FloorRef,
}

/// The result of resolving one [`ResourceRef`] — always one of these three
/// shapes, never a silently-guessed pick among several matches (section 5:
/// "do not silently choose one unless contextual resolution is
/// unambiguous by explicit rules").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResolveOutcome {
    Found { resource: ResolvedResource },
    Ambiguous { candidates: Vec<ResolvedResource> },
    NotFound,
}

/// One workspace's addressable data, as the resolver needs it: live or
/// dormant, it's the same shape (`store::WorkspaceRecord`'s own
/// `id`/`name`/`nodes`/`edges`) — see `App::workspace_views`, which builds
/// one of these for the active workspace and one for every dormant
/// workspace, so a reference can be disambiguated across all of them
/// (section 5's "duplicate names across Workspaces").
pub struct WorkspaceView {
    pub id: Uuid,
    pub name: String,
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
}

/// The caller's own workspace/floor (section 7), used only to narrow an
/// otherwise-ambiguous match — never to grant access. `None` means no
/// context is available (a bare human/CLI invocation), so ambiguity is
/// reported as-is with no guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveContext {
    pub workspace_id: Uuid,
    pub floor: FloorRef,
}

/// Basic, non-gated metadata for a resolved resource — `duetctl resource
/// inspect`'s answer. Deliberately NOT one generic structure across kinds
/// (section 6): each kind's own service already defines what "basic
/// metadata" means for it (`AgentInfo`, `NoteSummary`), and a `Note`'s
/// variant carries no Markdown content — reading that still goes through
/// `NoteService::read_note`, which enforces `ReadNote`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourceDetail {
    Agent(crate::message::AgentInfo),
    Note(crate::message::NoteSummary),
}

fn agent_candidates(view: &WorkspaceView, roles: &[Role]) -> Vec<ResolvedResource> {
    agent_identities(&view.nodes, view.id, roles)
        .into_iter()
        .map(|identity| ResolvedResource {
            kind: ResourceKind::Agent,
            id: identity.id,
            name: identity.name,
            workspace_id: view.id,
            workspace_name: view.name.clone(),
            floor: identity.floor,
        })
        .collect()
}

fn note_candidates(view: &WorkspaceView) -> Vec<ResolvedResource> {
    view.nodes
        .iter()
        .filter_map(|node| {
            let note = node.as_note()?;
            Some(ResolvedResource {
                kind: ResourceKind::Note,
                id: node.id,
                name: note_title(&note.markdown),
                workspace_id: view.id,
                workspace_name: view.name.clone(),
                floor: node.floor,
            })
        })
        .collect()
}

fn candidates_for_kind(
    kind: ResourceKind,
    view: &WorkspaceView,
    roles: &[Role],
) -> Vec<ResolvedResource> {
    match kind {
        ResourceKind::Agent => agent_candidates(view, roles),
        ResourceKind::Note => note_candidates(view),
    }
}

const ALL_KINDS: [ResourceKind; 2] = [ResourceKind::Agent, ResourceKind::Note];

/// Resolves `reference` against every workspace in `workspaces`. Matching is
/// case-insensitive on name (an `@backend` reference is meant to find an
/// agent literally named "Backend" — see the Milestone 5 acceptance script)
/// or exact on stable id, so a reference can always name a resource by its
/// id even after a rename (section 5: "human-readable names are not
/// identities"). `context`, if given, narrows — never silently picks among —
/// an otherwise-ambiguous result (section 7).
pub fn resolve(
    reference: &ResourceRef,
    workspaces: &[WorkspaceView],
    roles: &[Role],
    context: Option<&ResolveContext>,
) -> ResolveOutcome {
    let kinds: &[ResourceKind] = match &reference.kind {
        Some(kind) => std::slice::from_ref(kind),
        None => &ALL_KINDS,
    };
    let wanted_id = Uuid::parse_str(&reference.name).ok();

    let candidates: Vec<ResolvedResource> = workspaces
        .iter()
        .flat_map(|view| {
            kinds
                .iter()
                .flat_map(move |&kind| candidates_for_kind(kind, view, roles))
        })
        .filter(|candidate| {
            wanted_id == Some(candidate.id) || candidate.name.eq_ignore_ascii_case(&reference.name)
        })
        .collect();

    match candidates.len() {
        0 => ResolveOutcome::NotFound,
        1 => ResolveOutcome::Found {
            resource: candidates.into_iter().next().unwrap(),
        },
        _ => narrow_by_context(candidates, context),
    }
}

/// Narrows an ambiguous candidate list using the caller's own
/// workspace/floor: first to candidates in the caller's workspace, then (if
/// still ambiguous) to the caller's floor within that — only if either step
/// leaves exactly one candidate. Anything else stays `Ambiguous` with the
/// full, unfiltered candidate list, so the caller always sees every option
/// rather than a silently-narrowed subset.
fn narrow_by_context(
    candidates: Vec<ResolvedResource>,
    context: Option<&ResolveContext>,
) -> ResolveOutcome {
    if let Some(context) = context {
        let in_workspace: Vec<ResolvedResource> = candidates
            .iter()
            .filter(|candidate| candidate.workspace_id == context.workspace_id)
            .cloned()
            .collect();
        let narrowed = match in_workspace.len() {
            1 => in_workspace,
            n if n > 1 => in_workspace
                .into_iter()
                .filter(|candidate| candidate.floor == context.floor)
                .collect(),
            _ => Vec::new(),
        };
        if narrowed.len() == 1 {
            return ResolveOutcome::Found {
                resource: narrowed.into_iter().next().unwrap(),
            };
        }
    }
    ResolveOutcome::Ambiguous { candidates }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::model::{EnvironmentKind, NodeKind, NotePayload, NoteViewMode, TerminalPayload};

    fn agent_node(name: &str, floor: FloorRef) -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor,
            position: (0.0, 0.0),
            size: (1.0, 1.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Terminal(TerminalPayload {
                name: name.to_string(),
                cwd: std::path::PathBuf::from("/"),
                agent: Agent::Shell,
                claude_session_id: None,
                claude_account: None,
                never_launched: false,
                role_id: None,
                environment: EnvironmentKind::LocalPty,
            }),
        }
    }

    fn note_node(markdown: &str, floor: FloorRef) -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor,
            position: (0.0, 0.0),
            size: (1.0, 1.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Note(NotePayload {
                markdown: markdown.to_string(),
                color: "yellow".to_string(),
                view_mode: NoteViewMode::Preview,
            }),
        }
    }

    fn workspace(id: Uuid, name: &str, nodes: Vec<NodeRecord>) -> WorkspaceView {
        WorkspaceView {
            id,
            name: name.to_string(),
            nodes,
            edges: Vec::new(),
        }
    }

    #[test]
    fn parses_unqualified_and_qualified_references() {
        assert_eq!(
            ResourceRef::parse("@backend").unwrap(),
            ResourceRef {
                kind: None,
                name: "backend".to_string()
            }
        );
        assert_eq!(
            ResourceRef::parse("@agent:backend").unwrap(),
            ResourceRef {
                kind: Some(ResourceKind::Agent),
                name: "backend".to_string()
            }
        );
    }

    #[test]
    fn a_reference_without_a_leading_at_is_rejected() {
        assert!(ResourceRef::parse("backend").is_err());
    }

    #[test]
    fn an_unknown_qualifier_is_rejected() {
        assert!(ResourceRef::parse("@file:readme").is_err());
    }

    #[test]
    fn exact_agent_resolution() {
        let backend = agent_node("Backend", FloorRef::Ground);
        let id = backend.id;
        let views = vec![workspace(Uuid::new_v4(), "main", vec![backend])];
        let reference = ResourceRef::parse("@backend").unwrap();
        let outcome = resolve(&reference, &views, &[], None);
        assert_eq!(
            outcome,
            ResolveOutcome::Found {
                resource: ResolvedResource {
                    kind: ResourceKind::Agent,
                    id,
                    name: "Backend".to_string(),
                    workspace_id: views[0].id,
                    workspace_name: "main".to_string(),
                    floor: FloorRef::Ground,
                }
            }
        );
    }

    #[test]
    fn exact_note_resolution() {
        let note = note_node("# Requirements\n\nbody", FloorRef::Ground);
        let id = note.id;
        let views = vec![workspace(Uuid::new_v4(), "main", vec![note])];
        let reference = ResourceRef::parse("@requirements").unwrap();
        let outcome = resolve(&reference, &views, &[], None);
        match outcome {
            ResolveOutcome::Found { resource } => {
                assert_eq!(resource.kind, ResourceKind::Note);
                assert_eq!(resource.id, id);
                assert_eq!(resource.name, "Requirements");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn qualified_reference_restricts_to_its_kind() {
        // Same name as both an Agent and a Note; the qualified form must
        // pick exactly the one it names, with no ambiguity.
        let agent = agent_node("Backend", FloorRef::Ground);
        let note = note_node("# Backend", FloorRef::Ground);
        let views = vec![workspace(
            Uuid::new_v4(),
            "main",
            vec![agent.clone(), note.clone()],
        )];

        let as_agent = resolve(
            &ResourceRef::parse("@agent:backend").unwrap(),
            &views,
            &[],
            None,
        );
        assert_eq!(
            as_agent,
            ResolveOutcome::Found {
                resource: agent_candidates(&views[0], &[]).remove(0)
            }
        );

        let as_note = resolve(
            &ResourceRef::parse("@note:backend").unwrap(),
            &views,
            &[],
            None,
        );
        match as_note {
            ResolveOutcome::Found { resource } => assert_eq!(resource.id, note.id),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn unknown_resource_resolves_to_not_found() {
        let views = vec![workspace(Uuid::new_v4(), "main", vec![])];
        let outcome = resolve(&ResourceRef::parse("@ghost").unwrap(), &views, &[], None);
        assert_eq!(outcome, ResolveOutcome::NotFound);
    }

    #[test]
    fn ambiguous_resource_across_kinds_lists_every_candidate() {
        let agent = agent_node("Backend", FloorRef::Ground);
        let note = note_node("# Backend", FloorRef::Ground);
        let views = vec![workspace(Uuid::new_v4(), "main", vec![agent, note])];
        let outcome = resolve(&ResourceRef::parse("@backend").unwrap(), &views, &[], None);
        match outcome {
            ResolveOutcome::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_names_across_workspaces_are_ambiguous_without_context() {
        let a = agent_node("Backend", FloorRef::Ground);
        let b = agent_node("Backend", FloorRef::Ground);
        let views = vec![
            workspace(Uuid::new_v4(), "alpha", vec![a]),
            workspace(Uuid::new_v4(), "beta", vec![b]),
        ];
        let outcome = resolve(&ResourceRef::parse("@backend").unwrap(), &views, &[], None);
        match outcome {
            ResolveOutcome::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn caller_context_prefers_the_callers_own_workspace() {
        let a = agent_node("Backend", FloorRef::Ground);
        let b = agent_node("Backend", FloorRef::Ground);
        let a_id = a.id;
        let views = vec![
            workspace(Uuid::new_v4(), "alpha", vec![a]),
            workspace(Uuid::new_v4(), "beta", vec![b]),
        ];
        let context = ResolveContext {
            workspace_id: views[0].id,
            floor: FloorRef::Ground,
        };
        let outcome = resolve(
            &ResourceRef::parse("@backend").unwrap(),
            &views,
            &[],
            Some(&context),
        );
        match outcome {
            ResolveOutcome::Found { resource } => assert_eq!(resource.id, a_id),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_names_across_floors_resolved_by_context() {
        let floor_id = Uuid::new_v4();
        let ground = agent_node("Reviewer", FloorRef::Ground);
        let floor = agent_node("Reviewer", FloorRef::Floor(floor_id));
        let floor_agent_id = floor.id;
        let workspace_id = Uuid::new_v4();
        let views = vec![workspace(workspace_id, "main", vec![ground, floor])];

        // No context: both floors' agents are candidates.
        let unresolved = resolve(&ResourceRef::parse("@reviewer").unwrap(), &views, &[], None);
        assert!(matches!(unresolved, ResolveOutcome::Ambiguous { .. }));

        // Caller on the same floor: unambiguous.
        let context = ResolveContext {
            workspace_id,
            floor: FloorRef::Floor(floor_id),
        };
        let resolved = resolve(
            &ResourceRef::parse("@reviewer").unwrap(),
            &views,
            &[],
            Some(&context),
        );
        match resolved {
            ResolveOutcome::Found { resource } => assert_eq!(resource.id, floor_agent_id),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn stable_id_resolution_survives_a_rename() {
        let mut backend = agent_node("Backend", FloorRef::Ground);
        let id = backend.id;
        let views_before = vec![workspace(Uuid::new_v4(), "main", vec![backend.clone()])];
        let workspace_id = views_before[0].id;

        // Resolves by the old name while it's still current.
        assert!(matches!(
            resolve(
                &ResourceRef::parse("@backend").unwrap(),
                &views_before,
                &[],
                None
            ),
            ResolveOutcome::Found { .. }
        ));

        // Rename, as if the agent's terminal were renamed.
        backend.as_terminal_mut().unwrap().name = "Backend2".to_string();
        let views_after = vec![workspace(workspace_id, "main", vec![backend])];

        assert_eq!(
            resolve(
                &ResourceRef::parse("@backend").unwrap(),
                &views_after,
                &[],
                None
            ),
            ResolveOutcome::NotFound
        );
        match resolve(
            &ResourceRef::parse(&format!("@{id}")).unwrap(),
            &views_after,
            &[],
            None,
        ) {
            ResolveOutcome::Found { resource } => assert_eq!(resource.name, "Backend2"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn resolving_a_note_never_checks_capabilities() {
        // Resolution and access are different (section 8): an agent with no
        // ReadNote edge to this note at all must still be able to resolve
        // it — `authorize_note` (not `resolve`) is what refuses the actual
        // read. See `orchestration::notes::agent_without_a_read_note_edge_is_unauthorized`
        // for the access-side half of this guarantee.
        let note = note_node("# Secret Plan", FloorRef::Ground);
        let views = vec![workspace(Uuid::new_v4(), "main", vec![note.clone()])];
        let outcome = resolve(
            &ResourceRef::parse("@secret plan").unwrap(),
            &views,
            &[],
            None,
        );
        assert_eq!(
            outcome,
            ResolveOutcome::Found {
                resource: note_candidates(&views[0]).remove(0)
            }
        );
        assert!(
            super::super::notes::authorize_note(
                &[],
                Some(Uuid::new_v4()),
                note.id,
                crate::model::EdgeCapability::ReadNote,
            )
            .is_err()
        );
    }

    #[test]
    fn resolve_outcome_serializes_to_machine_readable_json() {
        let backend = agent_node("Backend", FloorRef::Ground);
        let id = backend.id;
        let views = vec![workspace(Uuid::new_v4(), "main", vec![backend])];
        let outcome = resolve(&ResourceRef::parse("@backend").unwrap(), &views, &[], None);
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["status"], "found");
        assert_eq!(json["resource"]["kind"], "agent");
        assert_eq!(json["resource"]["id"], id.to_string());

        let ambiguous = ResolveOutcome::Ambiguous { candidates: vec![] };
        assert_eq!(
            serde_json::to_value(&ambiguous).unwrap()["status"],
            "ambiguous"
        );
        assert_eq!(
            serde_json::to_value(&ResolveOutcome::NotFound).unwrap()["status"],
            "not_found"
        );
    }
}
