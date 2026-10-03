//! The structured agent-to-agent message domain model. Canvas links describe
//! which sessions are connected, while messages are explicit, addressed
//! deliveries with their own identity and delivery outcome. `control.rs`
//! writes a message into the target session's PTY input and exposes
//! sending/listing over a local socket.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// An `AgentMessage`'s lifecycle. `Queued` and `Delivered`/`Failed` are the
/// states `orchestration::bus::MessageBus` actually produces, driven by a
/// real attempt to write to the target's input. `Acknowledged` is kept
/// representable — a receiving agent has no way to signal "I've read this"
/// yet, so nothing ever assigns it this milestone — rather than invented
/// with false confidence; see `runtime::AgentActivity`'s doc comment for the
/// same "representable now, honestly unproduced until there's a reliable
/// signal" pattern already used elsewhere in this codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    Queued,
    Delivered,
    Acknowledged,
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

/// One agent, as `duetctl agents list`/`agents inspect` report it — richer
/// than `AgentSummary`: stable id, provider, assigned role (if any), whether
/// that role grants manager permissions, and its current `AgentActivity`
/// (formatted with `{:?}`, not a separate string enum — one representation
/// of the same `runtime::AgentActivity`, not two to keep in sync).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: Uuid,
    pub name: String,
    pub provider: String,
    pub role: Option<String>,
    pub manager: bool,
    pub activity: String,
}

/// One edge, as `duetctl connections list` reports it: both endpoints by id
/// and name, plus its granted capabilities (empty = visual-only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub id: Uuid,
    pub source_id: Uuid,
    pub source_name: String,
    pub target_id: Uuid,
    pub target_name: String,
    pub capabilities: Vec<String>,
}

/// The active workspace, as `duetctl workspace inspect` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: Uuid,
    pub name: String,
    pub root_dir: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub environment: String,
}

/// `duetctl whoami`'s answer: everything the `duet` skill (`orchestration::
/// skill`) tells an agent to look up about itself in one call, instead of
/// having its role instructions and connections resent as prompt text on
/// every launch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhoamiInfo {
    pub agent: AgentInfo,
    pub role_instructions: Option<String>,
    pub connected_agents: Vec<AgentInfo>,
    /// Project files and folders connected to this agent on the canvas (an
    /// Editor, a FileTree's root, or a file-backed note), as `@file:`
    /// references — context the user handed this agent to start from.
    #[serde(default)]
    pub connected_files: Vec<String>,
}

/// One note, as `duetctl notes list` reports it — just enough to pick a
/// target by id. `title` is a display convenience (`orchestration::notes::
/// note_title`), never an identity: a note has no name field, only its id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteSummary {
    pub id: Uuid,
    pub title: String,
    /// The project file a file-backed note is synced with (Milestone 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// One note's full content, as `duetctl notes read` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteDetail {
    pub id: Uuid,
    pub title: String,
    pub markdown: String,
    pub color: String,
    /// The project file a file-backed note is synced with (Milestone 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
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
