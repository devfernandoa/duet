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
//! `Agent` and `Note` (Milestone 5) resolve against workspace node records;
//! `File` and `Diff` (Milestone 6) resolve against the caller's project root
//! through `project::ProjectFilesystem`, so `@file:src/auth.rs` means "this
//! file in *my* workspace's project", never a guess across workspaces, and
//! can never reach outside that root (`project::ProjectPath` rejects
//! traversal at parse time; `LocalProject` rejects symlink escapes).
//! `ResourceKind` is deliberately a plain enum with one match per kind
//! inside this module, not a `dyn` provider registry — the one place this
//! crate is allowed to know about every resource kind at once. `Portal`
//! (Milestone 8) was added exactly that way: a variant, a candidate
//! function over `Portal` nodes' names, and one more entry in the kinds an
//! unqualified `@name` searches; `Floor`/`Workspace` will follow suit.
//!
//! File references accept an optional selection suffix,
//! `@file:src/auth.rs#L10-20`, and an *unqualified* reference that is
//! recognizably path-shaped (`@src/auth.rs`, `@README.md` — see
//! [`is_path_like`]) is also tried as a file. A plain word never is: `@auth`
//! only ever searches agents, notes and portals.

use super::identity::agent_identities;
use super::notes::note_title;
use crate::model::{EdgeRecord, FloorRef, NodeRecord};
use crate::project::Project;
use crate::project::git::GitService;
use crate::project::path::{LineRange, ProjectPath, split_line_suffix};
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
    /// A file or directory under the caller's project root.
    File,
    /// The uncommitted Git changes (working tree vs `HEAD`) of a file,
    /// directory, or — as `@diff:.` — the whole project.
    Diff,
    /// A browser Portal node (Milestone 8), by its name.
    Portal,
}

impl ResourceKind {
    /// Parses the qualifier before the `:` in `@kind:name` — case-insensitive
    /// since this is typed by humans and LLMs, not machine-generated.
    pub fn parse(raw: &str) -> Option<ResourceKind> {
        match raw.to_ascii_lowercase().as_str() {
            "agent" => Some(ResourceKind::Agent),
            "note" => Some(ResourceKind::Note),
            "file" => Some(ResourceKind::File),
            "diff" => Some(ResourceKind::Diff),
            "portal" => Some(ResourceKind::Portal),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ResourceKind::Agent => "agent",
            ResourceKind::Note => "note",
            ResourceKind::File => "file",
            ResourceKind::Diff => "diff",
            ResourceKind::Portal => "portal",
        }
    }

    /// Whether this kind lives in the project filesystem rather than in a
    /// workspace's node records.
    pub fn is_project_kind(&self) -> bool {
        matches!(self, ResourceKind::File | ResourceKind::Diff)
    }
}

/// Whether an unqualified reference body is recognizably a path, and so
/// should also be tried as a file: it contains a `/`, or it is a single word
/// with a file extension (`README.md`, `Cargo.toml`). Deliberately narrow —
/// an ordinary word (`@backend`, `@auth`) is never treated as a file.
pub fn is_path_like(name: &str) -> bool {
    let name = split_line_suffix(name)
        .map(|(path, _)| path)
        .unwrap_or(name);
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return false;
    }
    if name.contains('/') {
        return true;
    }
    match name.rsplit_once('.') {
        Some((stem, extension)) => {
            !stem.is_empty()
                && (1..=10).contains(&extension.len())
                && extension.starts_with(|c: char| c.is_ascii_alphabetic())
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// A parsed `@name` or `@kind:name` reference (section 2). Carries no
/// resolution state of its own — [`resolve`] is the only thing that turns
/// one of these into an actual resource.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRef {
    pub kind: Option<ResourceKind>,
    pub name: String,
    /// A `#L10-20` selection, only ever set for a file (qualified, or an
    /// unqualified path-like reference).
    pub lines: Option<LineRange>,
}

impl ResourceRef {
    /// Parses `@name`, `@kind:name`, `@file:path#L10-20`, or a path-like
    /// `@src/auth.rs`. The leading `@` is required — it's the explicit,
    /// deterministic signal section 10 asks for; plain words are never
    /// upgraded into a reference by this parser.
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
                    format!(
                        "unknown resource kind '{kind}' (expected agent, note, file, diff or portal)"
                    )
                })?;
                if name.is_empty() {
                    return Err(format!("'@{}:' needs a name after the colon", kind.label()));
                }
                let (name, lines) = match kind {
                    ResourceKind::File => split_line_suffix(name)?,
                    _ => (name, None),
                };
                if kind.is_project_kind() {
                    // Validated here, so `@file:../etc/passwd` is a clear
                    // error rather than a quiet "not found".
                    ProjectPath::parse(name).map_err(|error| error.to_string())?;
                }
                Ok(ResourceRef {
                    kind: Some(kind),
                    name: name.to_string(),
                    lines,
                })
            }
            None if is_path_like(body) => {
                let (name, lines) = split_line_suffix(body)?;
                Ok(ResourceRef {
                    kind: None,
                    name: name.to_string(),
                    lines,
                })
            }
            None => Ok(ResourceRef {
                kind: None,
                name: body.to_string(),
                lines: None,
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
    /// A node resource's own stable id. For a `File`/`Diff`, a
    /// deterministic id derived from the workspace and path (see
    /// [`project_resource_id`]) — the path itself is that resource's real
    /// identity, carried in `path`.
    pub id: Uuid,
    pub name: String,
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub floor: FloorRef,
    /// Project-relative path, for `File`/`Diff` resources only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The selected line range of a `File` reference, if one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
}

impl ResolvedResource {
    /// The canonical `@kind:...` form of this resource — what to pass on to
    /// another agent so it can resolve exactly the same thing.
    pub fn reference(&self) -> String {
        match (self.kind, &self.path) {
            (ResourceKind::File, Some(path)) => match self.lines {
                Some(lines) => format!("@file:{path}#{lines}"),
                None => format!("@file:{path}"),
            },
            (ResourceKind::Diff, Some(path)) => format!("@diff:{path}"),
            (kind, _) => format!("@{}:{}", kind.label(), self.id),
        }
    }
}

/// The project a reference's `File`/`Diff` candidates resolve against: the
/// caller's own workspace's root (CLAUDE.md, "resolution must be scoped to
/// the current Workspace/Floor project root"). Every file a caller can name
/// lives under this one root; there is no cross-workspace file resolution.
pub struct ProjectScope<'a> {
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub floor: FloorRef,
    pub project: &'a dyn Project,
}

/// A stable, deterministic id for a project-path resource (UUIDv5 of the
/// workspace id and `kind:path`): the same file always gets the same id, so
/// it can sit next to node ids in JSON output, but it is derived, never
/// stored — the path is the identity.
pub fn project_resource_id(workspace_id: Uuid, kind: ResourceKind, path: &ProjectPath) -> Uuid {
    Uuid::new_v5(
        &workspace_id,
        format!("{}:{}", kind.label(), path.display()).as_bytes(),
    )
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
    Portal(crate::message::PortalSummary),
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
            path: None,
            lines: None,
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
                path: None,
                lines: None,
            })
        })
        .collect()
}

fn portal_candidates(view: &WorkspaceView) -> Vec<ResolvedResource> {
    view.nodes
        .iter()
        .filter_map(|node| {
            let portal = node.as_portal()?;
            Some(ResolvedResource {
                kind: ResourceKind::Portal,
                id: node.id,
                name: portal.name.clone(),
                workspace_id: view.id,
                workspace_name: view.name.clone(),
                floor: node.floor,
                path: None,
                lines: None,
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
        ResourceKind::Portal => portal_candidates(view),
        // Project kinds don't come from node records; see `project_candidate`.
        ResourceKind::File | ResourceKind::Diff => Vec::new(),
    }
}

const NODE_KINDS: [ResourceKind; 3] = [
    ResourceKind::Agent,
    ResourceKind::Note,
    ResourceKind::Portal,
];

/// The `File`/`Diff` candidate `reference` names in `scope`, if any. A file
/// must exist; a diff must name the project root, an existing path, or a
/// path Git reports as changed (a deleted file still has a diff).
fn project_candidate(
    kind: ResourceKind,
    reference: &ResourceRef,
    scope: &ProjectScope,
) -> Option<ResolvedResource> {
    let path = ProjectPath::parse(&reference.name).ok()?;
    let exists = match kind {
        ResourceKind::File => !path.is_root() && scope.project.exists(&path),
        ResourceKind::Diff => {
            path.is_root()
                || scope.project.exists(&path)
                || GitService::new(scope.project)
                    .status()
                    .is_ok_and(|status| status.has_changes_under(&path))
        }
        ResourceKind::Agent | ResourceKind::Note | ResourceKind::Portal => false,
    };
    exists.then(|| ResolvedResource {
        kind,
        id: project_resource_id(scope.workspace_id, kind, &path),
        name: path.display().to_string(),
        workspace_id: scope.workspace_id,
        workspace_name: scope.workspace_name.clone(),
        floor: scope.floor,
        path: Some(path.display().to_string()),
        lines: if kind == ResourceKind::File {
            reference.lines
        } else {
            None
        },
    })
}

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
    resolve_with_project(reference, workspaces, roles, context, None)
}

/// [`resolve`], plus `File`/`Diff` resolution against `project` (the
/// caller's own project root). An unqualified reference is tried as a file
/// only when it [`is_path_like`]; if it *also* matches an agent or note by
/// name, the result is `Ambiguous`, never a silent pick.
pub fn resolve_with_project(
    reference: &ResourceRef,
    workspaces: &[WorkspaceView],
    roles: &[Role],
    context: Option<&ResolveContext>,
    project: Option<&ProjectScope>,
) -> ResolveOutcome {
    let node_kinds: &[ResourceKind] = match &reference.kind {
        Some(kind) if kind.is_project_kind() => &[],
        Some(kind) => std::slice::from_ref(kind),
        None => &NODE_KINDS,
    };
    let wanted_id = Uuid::parse_str(&reference.name).ok();

    let mut candidates: Vec<ResolvedResource> = workspaces
        .iter()
        .flat_map(|view| {
            node_kinds
                .iter()
                .flat_map(move |&kind| candidates_for_kind(kind, view, roles))
        })
        .filter(|candidate| {
            wanted_id == Some(candidate.id) || candidate.name.eq_ignore_ascii_case(&reference.name)
        })
        .collect();

    if let Some(scope) = project {
        let project_kind = match reference.kind {
            Some(kind) if kind.is_project_kind() => Some(kind),
            None if is_path_like(&reference.name) => Some(ResourceKind::File),
            _ => None,
        };
        if let Some(kind) = project_kind
            && let Some(candidate) = project_candidate(kind, reference, scope)
        {
            candidates.push(candidate);
        }
    }

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
                file: None,
            }),
        }
    }

    fn portal_node(name: &str, floor: FloorRef) -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor,
            position: (0.0, 0.0),
            size: (1.0, 1.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Portal(crate::model::PortalPayload::new(name, "")),
        }
    }

    #[test]
    fn portals_resolve_qualified_unqualified_and_report_ambiguity_like_other_kinds() {
        let frontend = portal_node("Frontend", FloorRef::Ground);
        let docs = portal_node("Docs", FloorRef::Ground);
        let admin_agent = agent_node("Admin", FloorRef::Ground);
        let admin_portal = portal_node("Admin", FloorRef::Ground);
        let views = vec![workspace(
            Uuid::new_v4(),
            "main",
            vec![
                frontend.clone(),
                docs,
                admin_agent.clone(),
                admin_portal.clone(),
            ],
        )];
        let resolve_ref = |raw: &str| resolve(&ResourceRef::parse(raw).unwrap(), &views, &[], None);

        // Qualified and (uniquely resolvable) unqualified forms.
        for raw in ["@portal:frontend", "@frontend", "@PORTAL:Frontend"] {
            let ResolveOutcome::Found { resource } = resolve_ref(raw) else {
                panic!("{raw}: expected Found");
            };
            assert_eq!(resource.kind, ResourceKind::Portal);
            assert_eq!(resource.id, frontend.id);
            assert_eq!(resource.reference(), format!("@portal:{}", frontend.id));
        }
        // By stable id, too.
        assert!(matches!(
            resolve_ref(&format!("@{}", frontend.id)),
            ResolveOutcome::Found { .. }
        ));
        // A name shared with an agent is ambiguous unqualified, exactly as
        // agent/note collisions are; qualifying picks one.
        let ResolveOutcome::Ambiguous { candidates } = resolve_ref("@admin") else {
            panic!("expected Ambiguous");
        };
        let kinds: Vec<_> = candidates.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![ResourceKind::Agent, ResourceKind::Portal]);
        let ResolveOutcome::Found { resource } = resolve_ref("@portal:admin") else {
            panic!("expected Found");
        };
        assert_eq!(resource.id, admin_portal.id);
        let ResolveOutcome::Found { resource } = resolve_ref("@agent:admin") else {
            panic!("expected Found");
        };
        assert_eq!(resource.id, admin_agent.id);
        assert_eq!(resolve_ref("@portal:nowhere"), ResolveOutcome::NotFound);
    }

    #[test]
    fn duplicate_portal_names_are_ambiguous_and_narrowed_only_by_context() {
        let a = portal_node("Frontend", FloorRef::Ground);
        let b = portal_node("Frontend", FloorRef::Ground);
        let c = portal_node("Frontend", FloorRef::Ground);
        let views = vec![
            workspace(Uuid::new_v4(), "alpha", vec![a.clone()]),
            workspace(Uuid::new_v4(), "beta", vec![b, c]),
        ];
        let reference = ResourceRef::parse("@portal:frontend").unwrap();
        let ResolveOutcome::Ambiguous { candidates } = resolve(&reference, &views, &[], None)
        else {
            panic!("expected Ambiguous");
        };
        assert_eq!(candidates.len(), 3);
        // The caller's own workspace has exactly one: narrowed.
        let alpha = ResolveContext {
            workspace_id: views[0].id,
            floor: FloorRef::Ground,
        };
        assert_eq!(
            resolve(&reference, &views, &[], Some(&alpha)),
            ResolveOutcome::Found {
                resource: portal_candidates(&views[0]).remove(0)
            }
        );
        // Two in the caller's workspace on the same floor: still ambiguous.
        let beta = ResolveContext {
            workspace_id: views[1].id,
            floor: FloorRef::Ground,
        };
        assert!(matches!(
            resolve(&reference, &views, &[], Some(&beta)),
            ResolveOutcome::Ambiguous { .. }
        ));
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
                name: "backend".to_string(),
                lines: None,
            }
        );
        assert_eq!(
            ResourceRef::parse("@agent:backend").unwrap(),
            ResourceRef {
                kind: Some(ResourceKind::Agent),
                name: "backend".to_string(),
                lines: None,
            }
        );
    }

    #[test]
    fn a_reference_without_a_leading_at_is_rejected() {
        assert!(ResourceRef::parse("backend").is_err());
    }

    #[test]
    fn an_unknown_qualifier_is_rejected() {
        assert!(ResourceRef::parse("@bogus:readme").is_err());
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
                    path: None,
                    lines: None,
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

    fn project_scope(project: &dyn Project, workspace_id: Uuid) -> ProjectScope<'_> {
        ProjectScope {
            workspace_id,
            workspace_name: "main".to_string(),
            floor: FloorRef::Ground,
            project,
        }
    }

    #[test]
    fn file_references_parse_with_and_without_qualifiers() {
        let qualified = ResourceRef::parse("@file:src/auth.rs").unwrap();
        assert_eq!(qualified.kind, Some(ResourceKind::File));
        assert_eq!(qualified.name, "src/auth.rs");
        assert_eq!(qualified.lines, None);

        let selected = ResourceRef::parse("@file:src/auth.rs#L10-20").unwrap();
        assert_eq!(selected.lines, Some(LineRange { start: 10, end: 20 }));

        let unqualified = ResourceRef::parse("@src/auth.rs#L3").unwrap();
        assert_eq!(unqualified.kind, None);
        assert_eq!(unqualified.name, "src/auth.rs");
        assert_eq!(unqualified.lines, Some(LineRange { start: 3, end: 3 }));

        let diff = ResourceRef::parse("@diff:.").unwrap();
        assert_eq!(diff.kind, Some(ResourceKind::Diff));

        assert!(ResourceRef::parse("@file:../etc/passwd").is_err());
        assert!(ResourceRef::parse("@file:/etc/passwd").is_err());
        assert!(ResourceRef::parse("@file:a.rs#Lnope").is_err());
    }

    #[test]
    fn only_path_shaped_words_are_path_like() {
        for path_like in [
            "src/auth.rs",
            "README.md",
            "Cargo.toml",
            "./x",
            "a/b",
            "src/auth.rs#L2",
        ] {
            assert!(is_path_like(path_like), "{path_like}");
        }
        for word in ["backend", "auth", "v1.2", "Secret Plan", "x.", ".hidden"] {
            assert!(!is_path_like(word), "{word}");
        }
    }

    #[test]
    fn file_references_resolve_within_the_project_root_only() {
        use crate::project::LocalProject;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/auth.rs"), "fn login() {}\n").unwrap();
        std::fs::write(tmp.path().join("README.md"), "# hi\n").unwrap();
        let project = LocalProject::new(tmp.path());
        let workspace_id = Uuid::new_v4();
        let views = vec![workspace(workspace_id, "main", vec![])];
        let scope = project_scope(&project, workspace_id);
        let resolve_file = |raw: &str| {
            resolve_with_project(
                &ResourceRef::parse(raw).unwrap(),
                &views,
                &[],
                None,
                Some(&scope),
            )
        };

        let ResolveOutcome::Found { resource } = resolve_file("@file:src/auth.rs#L1") else {
            panic!("expected Found");
        };
        assert_eq!(resource.kind, ResourceKind::File);
        assert_eq!(resource.path.as_deref(), Some("src/auth.rs"));
        assert_eq!(resource.lines, Some(LineRange { start: 1, end: 1 }));
        assert_eq!(resource.workspace_id, workspace_id);
        assert_eq!(resource.reference(), "@file:src/auth.rs#L1");
        assert_eq!(
            resource.id,
            project_resource_id(
                workspace_id,
                ResourceKind::File,
                &ProjectPath::parse("src/auth.rs").unwrap()
            )
        );

        // Unqualified but path-like.
        assert!(matches!(
            resolve_file("@src/auth.rs"),
            ResolveOutcome::Found { .. }
        ));
        assert!(matches!(
            resolve_file("@./README.md"),
            ResolveOutcome::Found { .. }
        ));
        // Missing files and plain words don't resolve as files.
        assert_eq!(
            resolve_file("@file:src/missing.rs"),
            ResolveOutcome::NotFound
        );
        assert_eq!(resolve_file("@auth"), ResolveOutcome::NotFound);
        // Without a project scope, files never resolve.
        assert_eq!(
            resolve(
                &ResourceRef::parse("@file:src/auth.rs").unwrap(),
                &views,
                &[],
                None
            ),
            ResolveOutcome::NotFound
        );
    }

    #[test]
    fn a_path_like_reference_that_also_names_a_note_is_ambiguous() {
        use crate::project::LocalProject;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plan.md"), "# plan\n").unwrap();
        let project = LocalProject::new(tmp.path());
        let workspace_id = Uuid::new_v4();
        let views = vec![workspace(
            workspace_id,
            "main",
            vec![note_node("# plan.md", FloorRef::Ground)],
        )];
        let scope = project_scope(&project, workspace_id);
        let outcome = resolve_with_project(
            &ResourceRef::parse("@plan.md").unwrap(),
            &views,
            &[],
            None,
            Some(&scope),
        );
        let ResolveOutcome::Ambiguous { candidates } = outcome else {
            panic!("expected Ambiguous");
        };
        let kinds: Vec<_> = candidates.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![ResourceKind::Note, ResourceKind::File]);
        // Qualifying picks exactly one.
        assert!(matches!(
            resolve_with_project(
                &ResourceRef::parse("@file:plan.md").unwrap(),
                &views,
                &[],
                None,
                Some(&scope)
            ),
            ResolveOutcome::Found { .. }
        ));
    }

    #[test]
    fn symlinks_out_of_the_root_do_not_resolve() {
        use crate::project::LocalProject;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "x").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            tmp.path().join("link.txt"),
        )
        .unwrap();
        let project = LocalProject::new(tmp.path());
        let scope = project_scope(&project, Uuid::new_v4());
        assert_eq!(
            resolve_with_project(
                &ResourceRef::parse("@file:link.txt").unwrap(),
                &[],
                &[],
                None,
                Some(&scope)
            ),
            ResolveOutcome::NotFound
        );
    }

    #[test]
    fn diff_references_resolve_for_the_root_existing_and_deleted_paths() {
        use crate::project::ProjectFilesystem;
        use crate::project::git::tests::{git, p, repo};
        let (tmp, project) = repo();
        git(tmp.path(), &["rm", "-q", "src/auth.rs"]);
        let scope = project_scope(&project, Uuid::new_v4());
        let diff = |raw: &str| {
            resolve_with_project(
                &ResourceRef::parse(raw).unwrap(),
                &[],
                &[],
                None,
                Some(&scope),
            )
        };
        let ResolveOutcome::Found { resource } = diff("@diff:.") else {
            panic!("expected Found");
        };
        assert_eq!(resource.reference(), "@diff:.");
        // Deleted from disk, but Git still has a diff for it.
        assert!(!project.exists(&p("src/auth.rs")));
        assert!(matches!(
            diff("@diff:src/auth.rs"),
            ResolveOutcome::Found { .. }
        ));
        assert_eq!(diff("@diff:nowhere.rs"), ResolveOutcome::NotFound);
    }
}
