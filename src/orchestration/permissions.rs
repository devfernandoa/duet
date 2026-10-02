//! Edge-capability authorization — the part of Milestone 3 that turns
//! `EdgeRecord::capabilities` (designed in Milestone 1, never checked by
//! anything until now) into an actual access-control decision. See
//! CLAUDE.md's product model: "Connections between nodes should eventually
//! represent explicit capabilities, not just visual lines."

use crate::model::{EdgeCapability, EdgeRecord};
use uuid::Uuid;

/// Whether `source` may exercise `capability` against `target`, given the
/// live edge set.
///
/// A granted capability is undirected: an edge recorded Lead→Backend with
/// `SendMessages` lets either side message the other. This is a deliberate
/// interpretation call, not an oversight — see the Milestone 3 report's
/// "Architecture notes" for why (the acceptance scenario's own topology has
/// no edge back from Reviewer to Lead, so a reply has to be able to flow
/// backward along the same edge the delegation traveled forward on, or the
/// scenario is structurally uncompletable without a human relay).
///
/// `source == target` is always authorized — an agent "messaging" or
/// "reading" its own state isn't a cross-agent operation a connection could
/// even be about.
pub fn authorize(
    edges: &[EdgeRecord],
    source: Uuid,
    target: Uuid,
    capability: EdgeCapability,
) -> Result<(), String> {
    if source == target {
        return Ok(());
    }
    let allowed = edges.iter().any(|edge| {
        edge.capabilities.contains(&capability)
            && ((edge.source == source && edge.target == target)
                || (edge.source == target && edge.target == source))
    });
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "not authorized: no {capability:?} connection between {source} and {target}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(source: Uuid, target: Uuid, capabilities: &[EdgeCapability]) -> EdgeRecord {
        EdgeRecord {
            id: Uuid::new_v4(),
            source,
            target,
            capabilities: capabilities.iter().copied().collect(),
        }
    }

    #[test]
    fn same_agent_is_always_authorized() {
        let id = Uuid::new_v4();
        assert!(authorize(&[], id, id, EdgeCapability::SendMessages).is_ok());
    }

    #[test]
    fn no_edge_at_all_is_unauthorized() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(authorize(&[], a, b, EdgeCapability::SendMessages).is_err());
    }

    #[test]
    fn visual_only_edge_grants_no_capability() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![edge(a, b, &[])];
        assert!(authorize(&edges, a, b, EdgeCapability::SendMessages).is_err());
    }

    #[test]
    fn granted_capability_is_undirected() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![edge(a, b, &[EdgeCapability::SendMessages])];
        assert!(authorize(&edges, a, b, EdgeCapability::SendMessages).is_ok());
        assert!(authorize(&edges, b, a, EdgeCapability::SendMessages).is_ok());
    }

    #[test]
    fn a_different_capability_on_the_same_edge_is_not_enough() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![edge(a, b, &[EdgeCapability::ReadNote])];
        assert!(authorize(&edges, a, b, EdgeCapability::SendMessages).is_err());
        assert!(authorize(&edges, a, b, EdgeCapability::ReadNote).is_ok());
    }

    #[test]
    fn unrelated_edges_do_not_leak_authorization() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![edge(a, b, &[EdgeCapability::SendMessages])];
        assert!(authorize(&edges, a, c, EdgeCapability::SendMessages).is_err());
        assert!(authorize(&edges, b, c, EdgeCapability::SendMessages).is_err());
    }
}
