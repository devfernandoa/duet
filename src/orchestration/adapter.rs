//! `AgentAdapter`: the provider-specific seam between an `AgentIdentity` and
//! its terminal/runtime — see CLAUDE.md's "Agent architecture" diagram
//! (`AgentIdentity ↕ AgentAdapter ↕ terminal/runtime`). `agent::Agent`
//! already owns the one thing that genuinely differs per provider today
//! (argv construction via `Agent::launch`), so every adapter here delegates
//! to it rather than duplicating it; what this trait adds is a uniform,
//! swappable seam for message formatting that stays provider-aware without
//! `bus.rs` itself ever matching on `Agent`, and that tests can replace
//! entirely with `FakeAgentAdapter` (section 4: "Add tests with fake agents
//! before depending on real Claude sessions"). Actual delivery mechanics —
//! writing to a PTY, with whatever timing a provider's TUI needs — stay
//! `App`'s job, driven by `bus::MessageBus::next_dispatchable`; that's
//! runtime/GTK-timer territory, not provider territory, so it isn't part of
//! this trait.
//!
//! Provider-specific `status`/`stop` methods are deliberately NOT part of
//! this trait yet: every provider's status/stop goes through the exact same
//! `SessionRuntime`/`environment::terminate` path today (none of them has a
//! divergent health-check or shutdown API), so adding the methods now would
//! be five identical bodies with no behavioral difference to justify the
//! trait. Add them when a provider's status or stop genuinely diverges
//! (e.g. a provider polled over an HTTP API instead of a PTY).

use crate::agent::{Agent, Launch, LaunchRequest, custom_launch};
use crate::message::message_envelope;

pub trait AgentAdapter {
    fn provider_name(&self) -> &'static str;
    fn launch(&self, request: LaunchRequest) -> Launch;
    /// Formats one message body as bytes ready to submit to this agent's
    /// input. Submission *timing* (how and when those bytes actually reach
    /// a live process) is deliberately not this trait's job — see
    /// `bus::DeliverySink` — since that's runtime/GTK-timer territory, the
    /// same for every provider, not provider-specific behavior.
    fn format_message(&self, source_label: &str, content: &str) -> Vec<u8> {
        message_envelope(source_label, content).into_bytes()
    }
}

pub struct ClaudeAdapter;
pub struct CodexAdapter;
pub struct OpenCodeAdapter;
pub struct ShellAdapter;
pub struct CustomCommandAdapter {
    pub program: String,
    pub args: Vec<String>,
}

impl AgentAdapter for ClaudeAdapter {
    fn provider_name(&self) -> &'static str {
        "Claude"
    }
    fn launch(&self, request: LaunchRequest) -> Launch {
        Agent::Claude.launch(request)
    }
}

impl AgentAdapter for CodexAdapter {
    fn provider_name(&self) -> &'static str {
        "Codex"
    }
    fn launch(&self, request: LaunchRequest) -> Launch {
        Agent::Codex.launch(request)
    }
}

impl AgentAdapter for OpenCodeAdapter {
    fn provider_name(&self) -> &'static str {
        "OpenCode"
    }
    fn launch(&self, request: LaunchRequest) -> Launch {
        Agent::OpenCode.launch(request)
    }
}

impl AgentAdapter for ShellAdapter {
    fn provider_name(&self) -> &'static str {
        "Shell"
    }
    fn launch(&self, request: LaunchRequest) -> Launch {
        Agent::Shell.launch(request)
    }
}

impl AgentAdapter for CustomCommandAdapter {
    fn provider_name(&self) -> &'static str {
        "Custom"
    }
    fn launch(&self, _request: LaunchRequest) -> Launch {
        custom_launch(&self.program, &self.args)
    }
}

/// Picks the adapter for a `TerminalPayload`'s configured provider — the one
/// place that maps `Agent` to its `AgentAdapter`, so no caller re-matches
/// the provider enum itself.
pub fn adapter_for(agent: &Agent) -> Box<dyn AgentAdapter> {
    match agent {
        Agent::Claude => Box::new(ClaudeAdapter),
        Agent::Codex => Box::new(CodexAdapter),
        Agent::OpenCode => Box::new(OpenCodeAdapter),
        Agent::Shell => Box::new(ShellAdapter),
        Agent::Custom { program, args } => Box::new(CustomCommandAdapter {
            program: program.clone(),
            args: args.clone(),
        }),
    }
}

/// A fully in-memory stand-in for a real terminal-backed agent: formats
/// messages the same way a real adapter would, records what it was "sent"
/// instead of writing to a PTY, and can be told in advance to fail — so
/// orchestration tests (routing, permissions, queueing, failures, manager
/// actions) never need a real `claude`/`codex` process.
#[derive(Debug, Default)]
pub struct FakeAgentAdapter {
    pub received: Vec<Vec<u8>>,
    pub fail_delivery: bool,
}

impl FakeAgentAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn failing() -> Self {
        FakeAgentAdapter {
            received: Vec::new(),
            fail_delivery: true,
        }
    }

    /// Simulates attempting delivery of one message: records it (so a test
    /// can assert ordering/content) and reports success/failure the same
    /// way a real `write_input` call would. `false` whenever `fail_delivery`
    /// is set, so a bus test can exercise the `Failed` path deterministically.
    pub fn deliver(&mut self, source_label: &str, content: &str) -> bool {
        if self.fail_delivery {
            return false;
        }
        self.received
            .push(self.format_message(source_label, content));
        true
    }
}

impl AgentAdapter for FakeAgentAdapter {
    fn provider_name(&self) -> &'static str {
        "Fake"
    }
    fn launch(&self, _request: LaunchRequest) -> Launch {
        Launch {
            program: "true".to_string(),
            args: Vec::new(),
            envs: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_for_dispatches_to_the_matching_provider() {
        assert_eq!(adapter_for(&Agent::Claude).provider_name(), "Claude");
        assert_eq!(adapter_for(&Agent::Codex).provider_name(), "Codex");
        assert_eq!(adapter_for(&Agent::OpenCode).provider_name(), "OpenCode");
        assert_eq!(adapter_for(&Agent::Shell).provider_name(), "Shell");
        let custom = Agent::Custom {
            program: "mytool".to_string(),
            args: vec!["--flag".to_string()],
        };
        let adapter = adapter_for(&custom);
        assert_eq!(adapter.provider_name(), "Custom");
        assert_eq!(adapter.launch(LaunchRequest::default()).program, "mytool");
    }

    #[test]
    fn format_message_matches_the_shared_envelope_format() {
        let adapter = ClaudeAdapter;
        let bytes = adapter.format_message("lead (Claude)", "hello");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "[duet message from lead (Claude)]: hello"
        );
    }
}
