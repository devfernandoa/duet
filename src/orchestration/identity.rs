//! `AgentIdentity`: an agent's stable identity, independent of the GTK
//! terminal widget that happens to display it right now — see CLAUDE.md's
//! "Agent architecture" section. Computed on demand from a workspace's own
//! `NodeRecord`s rather than persisted separately: this milestone has
//! exactly one agent per terminal node, so the node's own id is already a
//! stable, unique identity and inventing a second one would just be a
//! parallel id to keep in sync for no behavioral gain. `terminal_id` is kept
//! as its own field (equal to `id` today) so a future milestone where an
//! agent can survive a terminal being replaced doesn't need another
//! migration to separate them.

use crate::agent::Agent;
use crate::model::{FloorRef, NodeRecord};
use crate::role::Role;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentIdentity {
    pub id: Uuid,
    pub terminal_id: Uuid,
    pub workspace_id: Uuid,
    pub floor: FloorRef,
    pub name: String,
    pub provider: Agent,
    pub role: Option<Role>,
}

impl AgentIdentity {
    /// What a message envelope should call this agent — the same
    /// "name (Provider)" shape `app::send_message` already used before this
    /// module existed.
    pub fn display_label(&self) -> String {
        format!("{} ({})", self.name, self.provider.display_name())
    }

    /// Whether this agent's assigned role grants manager/recruiting
    /// permissions (section 11: "do not give recruitment permissions to
    /// every agent").
    pub fn is_manager(&self) -> bool {
        self.role.as_ref().is_some_and(|role| role.manager)
    }
}

/// Every agent identity a workspace's live `Terminal` nodes currently
/// represent, in no particular order. `roles` is the full merged roster
/// (built-in plus custom) — the same list `App::roles()` already builds —
/// passed in rather than looked up here so this stays pure data logic with
/// no dependency on `App`.
pub fn agent_identities(
    nodes: &[NodeRecord],
    workspace_id: Uuid,
    roles: &[Role],
) -> Vec<AgentIdentity> {
    nodes
        .iter()
        .filter_map(|node| {
            let terminal = node.as_terminal()?;
            let role = terminal
                .role_id
                .and_then(|id| roles.iter().find(|role| role.id == id).cloned());
            Some(AgentIdentity {
                id: node.id,
                terminal_id: node.id,
                workspace_id,
                floor: node.floor,
                name: terminal.name.clone(),
                provider: terminal.agent.clone(),
                role,
            })
        })
        .collect()
}

/// Resolves a `duetctl`-style target: an exact id first, falling back to an
/// exact (case-sensitive) name match — the same resolution order
/// `app::App::find_session_id` already used.
pub fn find_identity<'a>(
    identities: &'a [AgentIdentity],
    needle: &str,
) -> Option<&'a AgentIdentity> {
    if let Ok(id) = Uuid::parse_str(needle)
        && let Some(found) = identities.iter().find(|identity| identity.id == id)
    {
        return Some(found);
    }
    identities.iter().find(|identity| identity.name == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EnvironmentKind, NodeKind, NotePayload, NoteViewMode, TerminalPayload};
    use std::path::PathBuf;

    fn terminal_node(name: &str, role_id: Option<Uuid>) -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (1.0, 1.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Terminal(TerminalPayload {
                name: name.to_string(),
                cwd: PathBuf::from("/"),
                agent: Agent::Claude,
                claude_session_id: None,
                claude_account: None,
                role_id,
                environment: EnvironmentKind::LocalPty,
            }),
        }
    }

    #[test]
    fn non_terminal_nodes_are_not_agents() {
        let note = NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (1.0, 1.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Note(NotePayload {
                markdown: String::new(),
                color: "yellow".to_string(),
                view_mode: NoteViewMode::Preview,
            }),
        };
        assert!(agent_identities(&[note], Uuid::new_v4(), &[]).is_empty());
    }

    #[test]
    fn identity_carries_its_resolved_role_and_display_label() {
        let role = Role {
            id: Uuid::new_v4(),
            name: "Lead".to_string(),
            instructions: "Coordinate.".to_string(),
            icon: None,
            accent: None,
            manager: true,
        };
        let node = terminal_node("lead", Some(role.id));
        let identities = agent_identities(&[node], Uuid::new_v4(), &[role]);
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].display_label(), "lead (Claude)");
        assert!(identities[0].is_manager());
    }

    #[test]
    fn find_identity_resolves_by_id_then_falls_back_to_name() {
        let node = terminal_node("backend", None);
        let identities = agent_identities(std::slice::from_ref(&node), Uuid::new_v4(), &[]);
        assert_eq!(
            find_identity(&identities, &node.id.to_string()).unwrap().id,
            node.id
        );
        assert_eq!(find_identity(&identities, "backend").unwrap().id, node.id);
        assert!(find_identity(&identities, "missing").is_none());
    }
}
