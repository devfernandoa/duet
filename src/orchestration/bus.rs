//! `MessageBus`: routing, permission-checking and per-agent queueing for
//! agent-to-agent messages — the orchestration/application service CLAUDE.md
//! asks for ("Build this as an application/orchestration service, not as GTK
//! callbacks"). Pure data and logic, no PTY/glib/GTK anywhere in this file:
//! `App` drives actual delivery (writing to a PTY, with correct timing) by
//! calling `next_dispatchable`/`mark_delivered`/`mark_failed`/
//! `cancel_dispatch` from its own poll loop; tests drive the exact same
//! methods with a `FakeAgentAdapter` standing in for the PTY, so routing,
//! permissions, queueing and failure handling are fully verifiable with no
//! real agent process (section 4).

use super::permissions::authorize;
use super::registry::AgentRegistry;
use crate::message::{AgentMessage, DeliveryStatus, now_epoch_secs};
use crate::model::EdgeCapability;
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

/// Capped the same way `App::messages` was before this module replaced it —
/// enough history to answer "what was just sent", not an unbounded log.
const LOG_LIMIT: usize = 200;

#[derive(Debug, Default)]
struct Inbox {
    queue: VecDeque<AgentMessage>,
    /// Set while one message from this inbox is out for delivery, so a
    /// second queued message is never dispatched before the first one's
    /// submission has actually resolved — section 9: "do not inject random
    /// text into terminal stdin at unpredictable times."
    delivering: bool,
}

#[derive(Debug, Default)]
pub struct MessageBus {
    inboxes: HashMap<Uuid, Inbox>,
    log: Vec<AgentMessage>,
}

impl MessageBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates and queues one message; does not deliver it — see
    /// `next_dispatchable`. Permission is only checked when `source` is
    /// `Some`: a message with no identified sender (today: `duet agent
    /// send`/`duetctl send` run from a bare shell with no `DUET_AGENT_ID`)
    /// is the human operator acting with the same trust the GTK UI already
    /// has, matching this codebase's behavior from before Milestone 3.
    pub fn send(
        &mut self,
        registry: &AgentRegistry,
        source: Option<Uuid>,
        target: Uuid,
        content: String,
    ) -> Result<AgentMessage, String> {
        if registry.get_agent(target).is_none() {
            return Err(format!("no agent named '{target}'"));
        }
        if let Some(source) = source {
            authorize(
                registry.edges(),
                source,
                target,
                EdgeCapability::SendMessages,
            )?;
        }
        let message = AgentMessage {
            id: Uuid::new_v4(),
            source,
            target,
            content,
            timestamp: now_epoch_secs(),
            status: DeliveryStatus::Queued,
        };
        self.inboxes
            .entry(target)
            .or_default()
            .queue
            .push_back(message.clone());
        self.push_log(message.clone());
        Ok(message)
    }

    /// At most one message per agent whose inbox isn't already mid-delivery
    /// — the unit `App`'s poll loop dispatches per tick. Each returned
    /// message stays `Queued` at the front of its inbox until the caller
    /// reports back via `mark_delivered`, `mark_failed`, or (the target
    /// turned out to be offline) `cancel_dispatch`.
    pub fn next_dispatchable(&mut self) -> Vec<AgentMessage> {
        self.inboxes
            .values_mut()
            .filter_map(|inbox| {
                if inbox.delivering {
                    return None;
                }
                let message = inbox.queue.front().cloned()?;
                inbox.delivering = true;
                Some(message)
            })
            .collect()
    }

    /// Delivery actually happened: pops the message and records it
    /// `Delivered`.
    pub fn mark_delivered(&mut self, message_id: Uuid) {
        self.resolve(message_id, DeliveryStatus::Delivered);
    }

    /// Delivery was genuinely attempted and failed (the target was online
    /// but the write itself failed) — distinct from `cancel_dispatch`, which
    /// is for a target that was never attempted at all.
    pub fn mark_failed(&mut self, message_id: Uuid) {
        self.resolve(message_id, DeliveryStatus::Failed);
    }

    /// The target wasn't actually attempted (e.g. it's offline right now):
    /// clears the in-flight flag without popping the message or changing
    /// its status, so the exact same `Queued` message is handed out again
    /// by a later `next_dispatchable` call once the target reconnects —
    /// section 9's "reconnect behavior".
    pub fn cancel_dispatch(&mut self, message_id: Uuid) {
        for inbox in self.inboxes.values_mut() {
            if inbox
                .queue
                .front()
                .is_some_and(|message| message.id == message_id)
            {
                inbox.delivering = false;
                break;
            }
        }
    }

    fn resolve(&mut self, message_id: Uuid, status: DeliveryStatus) {
        for inbox in self.inboxes.values_mut() {
            if inbox
                .queue
                .front()
                .is_some_and(|message| message.id == message_id)
            {
                inbox.queue.pop_front();
                inbox.delivering = false;
                break;
            }
        }
        if let Some(entry) = self.log.iter_mut().find(|message| message.id == message_id) {
            entry.status = status;
        }
    }

    /// How many messages are still waiting in `agent`'s inbox (including
    /// one currently mid-delivery) — `orchestration::activity`'s
    /// `AwaitingAgent` signal.
    pub fn inbox_len(&self, agent: Uuid) -> usize {
        self.inboxes
            .get(&agent)
            .map_or(0, |inbox| inbox.queue.len())
    }

    pub fn recent(&self) -> &[AgentMessage] {
        &self.log
    }

    fn push_log(&mut self, message: AgentMessage) {
        self.log.push(message);
        if self.log.len() > LOG_LIMIT {
            let overflow = self.log.len() - LOG_LIMIT;
            self.log.drain(0..overflow);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::model::{EdgeRecord, FloorRef};
    use crate::orchestration::adapter::FakeAgentAdapter;
    use crate::orchestration::identity::AgentIdentity;

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

    fn connected(a: Uuid, b: Uuid) -> EdgeRecord {
        EdgeRecord {
            id: Uuid::new_v4(),
            source: a,
            target: b,
            capabilities: [EdgeCapability::SendMessages].into_iter().collect(),
        }
    }

    #[test]
    fn send_to_an_unknown_agent_is_rejected_and_queues_nothing() {
        let registry = AgentRegistry::new(Vec::new(), Vec::new());
        let mut bus = MessageBus::new();
        let err = bus
            .send(&registry, None, Uuid::new_v4(), "hi".to_string())
            .unwrap_err();
        assert!(err.contains("no agent"));
        assert!(bus.recent().is_empty());
    }

    #[test]
    fn unauthorized_sender_is_rejected_before_queueing() {
        let (lead, reviewer) = (identity("lead"), identity("reviewer"));
        // No edge between them at all.
        let registry = AgentRegistry::new(vec![lead.clone(), reviewer.clone()], Vec::new());
        let mut bus = MessageBus::new();
        let err = bus
            .send(&registry, Some(lead.id), reviewer.id, "hi".to_string())
            .unwrap_err();
        assert!(err.contains("not authorized"));
        assert_eq!(bus.inbox_len(reviewer.id), 0);
    }

    #[test]
    fn a_human_operator_with_no_source_bypasses_edge_checks() {
        let agent = identity("lead");
        let registry = AgentRegistry::new(vec![agent.clone()], Vec::new());
        let mut bus = MessageBus::new();
        assert!(
            bus.send(&registry, None, agent.id, "hi".to_string())
                .is_ok()
        );
    }

    #[test]
    fn routing_respects_the_edge_between_sender_and_recipient() {
        let (lead, backend) = (identity("lead"), identity("backend"));
        let edges = vec![connected(lead.id, backend.id)];
        let registry = AgentRegistry::new(vec![lead.clone(), backend.clone()], edges);
        let mut bus = MessageBus::new();
        let message = bus
            .send(
                &registry,
                Some(lead.id),
                backend.id,
                "inspect the backend".to_string(),
            )
            .unwrap();
        assert_eq!(message.status, DeliveryStatus::Queued);
        assert_eq!(bus.inbox_len(backend.id), 1);
    }

    #[test]
    fn queueing_and_ordering_serializes_delivery_per_agent() {
        let (lead, backend) = (identity("lead"), identity("backend"));
        let edges = vec![connected(lead.id, backend.id)];
        let registry = AgentRegistry::new(vec![lead.clone(), backend.clone()], edges);
        let mut bus = MessageBus::new();
        let first = bus
            .send(&registry, Some(lead.id), backend.id, "first".to_string())
            .unwrap();
        let second = bus
            .send(&registry, Some(lead.id), backend.id, "second".to_string())
            .unwrap();
        assert_eq!(bus.inbox_len(backend.id), 2);

        // Only the first message is handed out while nothing has resolved.
        let ready = bus.next_dispatchable();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, first.id);
        assert!(bus.next_dispatchable().is_empty());

        bus.mark_delivered(first.id);
        let ready = bus.next_dispatchable();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, second.id);
        bus.mark_delivered(second.id);
        assert_eq!(bus.inbox_len(backend.id), 0);
    }

    #[test]
    fn delivery_failure_is_recorded_and_does_not_wedge_the_queue() {
        let (lead, backend) = (identity("lead"), identity("backend"));
        let edges = vec![connected(lead.id, backend.id)];
        let registry = AgentRegistry::new(vec![lead.clone(), backend.clone()], edges);
        let mut bus = MessageBus::new();
        let message = bus
            .send(&registry, Some(lead.id), backend.id, "hi".to_string())
            .unwrap();
        let mut fake = FakeAgentAdapter::failing();
        let ready = bus.next_dispatchable();
        assert_eq!(ready.len(), 1);
        let delivered = fake.deliver(&lead.display_label(), &ready[0].content);
        assert!(!delivered);
        bus.mark_failed(message.id);
        assert_eq!(
            bus.recent()
                .iter()
                .find(|m| m.id == message.id)
                .unwrap()
                .status,
            DeliveryStatus::Failed
        );
        assert_eq!(bus.inbox_len(backend.id), 0);
        assert!(fake.received.is_empty());
    }

    #[test]
    fn an_offline_agent_keeps_the_message_queued_until_it_reconnects() {
        let (lead, backend) = (identity("lead"), identity("backend"));
        let edges = vec![connected(lead.id, backend.id)];
        let registry = AgentRegistry::new(vec![lead.clone(), backend.clone()], edges);
        let mut bus = MessageBus::new();
        let message = bus
            .send(&registry, Some(lead.id), backend.id, "hi".to_string())
            .unwrap();

        // backend is offline: the caller never even attempts delivery.
        let ready = bus.next_dispatchable();
        assert_eq!(ready.len(), 1);
        bus.cancel_dispatch(message.id);
        assert_eq!(bus.inbox_len(backend.id), 1); // still queued, not failed
        assert_eq!(
            bus.recent()
                .iter()
                .find(|m| m.id == message.id)
                .unwrap()
                .status,
            DeliveryStatus::Queued
        );

        // backend reconnects: the same message is handed out again and this
        // time delivery succeeds.
        let ready = bus.next_dispatchable();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, message.id);
        let mut fake = FakeAgentAdapter::new();
        assert!(fake.deliver(&lead.display_label(), &ready[0].content));
        bus.mark_delivered(message.id);
        assert_eq!(bus.inbox_len(backend.id), 0);
    }
}
