//! The structured agent-to-agent message domain model. Canvas links describe
//! which sessions are connected, while messages are explicit, addressed
//! deliveries with their own identity and delivery outcome. `control.rs`
//! writes a message into the target session's PTY input and exposes
//! sending/listing over a local socket.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    Delivered,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: Uuid,
    /// The sending session, when it could be identified (a CLI invocation
    /// with no `DUET_SESSION_ID` in its environment — e.g. run from a plain
    /// terminal rather than from inside a duet-launched session — sends
    /// with `None`).
    pub source: Option<Uuid>,
    pub target: Uuid,
    pub content: String,
    pub timestamp: u64,
    pub status: DeliveryStatus,
}

/// Formats a message as terminal input. A single trailing carriage return is
/// the terminal representation of the Return key, which submits the message
/// in interactive agent UIs.
pub fn message_envelope(source_label: &str, content: &str) -> String {
    format!("[duet message from {source_label}]: {content}")
}

/// Seconds since the Unix epoch, for `AgentMessage::timestamp`. Falls back
/// to 0 only if the system clock is somehow set before 1970 — never fails.
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// One live session, as exposed by `duet agent list` — just enough to pick
/// a target by name or id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: Uuid,
    pub name: String,
    pub agent: String,
}

/// One canvas link, by the connected sessions' names — what "canvas
/// connections expose which agents are logically connected" means for
/// `duet agent list`'s output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkSummary {
    pub source: String,
    pub target: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_epoch_secs_is_plausibly_current() {
        // Any time after this commit was written; catches an accidental
        // `unwrap_or(0)` fallback firing on a normal clock.
        assert!(now_epoch_secs() > 1_700_000_000);
    }

    /// No trailing `\r` here — the submitting keystroke is written as its
    /// own, separately-timed `write_input` call (see `app::MESSAGE_SUBMIT_DELAY`),
    /// not appended to the envelope text itself.
    #[test]
    fn message_envelope_has_no_trailing_submit_byte() {
        assert_eq!(
            message_envelope("sender (Codex)", "Please review this."),
            "[duet message from sender (Codex)]: Please review this."
        );
    }
}
