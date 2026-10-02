//! `AgentRegistry`: a read-only snapshot of every agent and connection in a
//! workspace, queried identically by GTK, `duetctl`, and tests. Built fresh
//! from `App`'s own `nodes`/`edges`/roles whenever one is needed (`App`
//! already recomputes similarly-shaped summaries like `agent_summaries` on
//! demand) rather than kept permanently in sync — there is exactly one
//! source of truth (the node/edge records) and this is a view over it, not
//! a second copy that could drift.

use super::identity::{AgentIdentity, find_identity};
use crate::model::EdgeRecord;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    identities: Vec<AgentIdentity>,
    edges: Vec<EdgeRecord>,
}

impl AgentRegistry {
    pub fn new(identities: Vec<AgentIdentity>, edges: Vec<EdgeRecord>) -> Self {
        AgentRegistry { identities, edges }
    }

    pub fn list_agents(&self) -> &[AgentIdentity] {
        &self.identities
    }

    pub fn find_agent(&self, name: &str) -> Option<&AgentIdentity> {
        self.identities
            .iter()
            .find(|identity| identity.name == name)
    }

    pub fn get_agent(&self, id: Uuid) -> Option<&AgentIdentity> {
        self.identities.iter().find(|identity| identity.id == id)
    }

    /// Resolves a `duetctl`-style target: an id first, then an exact name.
    pub fn resolve(&self, needle: &str) -> Option<&AgentIdentity> {
        find_identity(&self.identities, needle)
    }

    /// Every other agent this one shares any edge with, regardless of
    /// capability — "is there a line between these two nodes at all",
    /// separate from whether that line grants any particular capability
    /// (see `permissions::authorize` for that question).
    pub fn connected_agents(&self, id: Uuid) -> Vec<&AgentIdentity> {
        self.edges
            .iter()
            .filter_map(|edge| {
                if edge.source == id {
                    self.get_agent(edge.target)
                } else if edge.target == id {
                    self.get_agent(edge.source)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn edges(&self) -> &[EdgeRecord] {
        &self.edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::model::{EdgeCapability, FloorRef};

    fn identity(name: &str) -> AgentIdentity {
        AgentIdentity {
            id: Uuid::new_v4(),
            terminal_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            name: name.to_string(),
            provider: Agent::Claude,
            role: None,
        }
    }

    #[test]
    fn resolves_by_id_or_name_and_reports_unknown_targets_as_none() {
        let lead = identity("lead");
        let registry = AgentRegistry::new(vec![lead.clone()], Vec::new());
        assert_eq!(registry.resolve(&lead.id.to_string()).unwrap().id, lead.id);
        assert_eq!(registry.resolve("lead").unwrap().id, lead.id);
        assert!(registry.resolve("ghost").is_none());
        assert!(registry.find_agent("ghost").is_none());
        assert!(registry.get_agent(Uuid::new_v4()).is_none());
    }

    #[test]
    fn connected_agents_follows_edges_in_either_direction() {
        let (lead, backend, frontend) =
            (identity("lead"), identity("backend"), identity("frontend"));
        let edges = vec![
            EdgeRecord {
                id: Uuid::new_v4(),
                source: lead.id,
                target: backend.id,
                capabilities: [EdgeCapability::SendMessages].into_iter().collect(),
            },
            EdgeRecord {
                id: Uuid::new_v4(),
                source: frontend.id,
                target: lead.id,
                capabilities: Default::default(),
            },
        ];
        let registry =
            AgentRegistry::new(vec![lead.clone(), backend.clone(), frontend.clone()], edges);
        let mut connected: Vec<Uuid> = registry
            .connected_agents(lead.id)
            .iter()
            .map(|a| a.id)
            .collect();
        connected.sort();
        let mut expected = vec![backend.id, frontend.id];
        expected.sort();
        assert_eq!(connected, expected);
        assert!(
            registry
                .connected_agents(backend.id)
                .iter()
                .any(|a| a.id == lead.id)
        );
    }
}
