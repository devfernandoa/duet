//! Agent-launch environment and discovery prompt — section 8: a launched
//! agent should be able to find and talk to the rest of the workspace
//! through `duetctl` without anything hand-fed to it first.

use super::identity::AgentIdentity;
use crate::agent::Launch;
use crate::model::FloorRef;
use std::path::Path;

/// One short paragraph telling a freshly-launched agent that `duetctl`
/// exists and how to use it — section 8: "Keep injected context compact."
/// Sent only once, on a terminal's actual first-ever launch (see
/// `model::TerminalPayload::never_launched`), and only for providers with
/// no installed Duet skill to rely on instead (OpenCode/Shell/Custom — see
/// `skill.rs`'s doc comment; Claude and Codex both get the skill). A
/// launch with its skill installed gets no prompt at all: the skill's own
/// description ("use at the very start of every session, before
/// responding to anything else") is what's supposed to make it
/// self-trigger, the same way any other skill does — a synthetic nudge on
/// top of that would just be a redundant extra turn.
pub const DISCOVERY_INSTRUCTION: &str = "You are running inside Duet, a multi-agent workspace. \
Run `duetctl whoami` to see your own role and who you're connected to, `duetctl agents list` \
to see every agent, and `duetctl send --to <agent> \"message\"` to message one you're connected \
to (your id is already in $DUET_AGENT_ID).";

/// The `DUET_*` variables section 8 asks every launched agent to receive.
/// `DUET_FLOOR_ID` is `"ground"` until Milestone 9 introduces real floors —
/// there is no other value `FloorRef::Ground` could mean yet.
pub fn env_vars(
    identity: &AgentIdentity,
    workspace_name: &str,
    socket_path: &Path,
) -> Vec<(String, String)> {
    let floor = match identity.floor {
        FloorRef::Ground => "ground".to_string(),
        FloorRef::Floor(id) => id.to_string(),
    };
    vec![
        (
            "DUET_WORKSPACE_ID".to_string(),
            identity.workspace_id.to_string(),
        ),
        (
            "DUET_WORKSPACE_NAME".to_string(),
            workspace_name.to_string(),
        ),
        (
            "DUET_TERMINAL_ID".to_string(),
            identity.terminal_id.to_string(),
        ),
        ("DUET_AGENT_ID".to_string(), identity.id.to_string()),
        (
            "DUET_ROLE".to_string(),
            identity
                .role
                .as_ref()
                .map(|role| role.name.clone())
                .unwrap_or_default(),
        ),
        ("DUET_FLOOR_ID".to_string(), floor),
        (
            "DUET_CONTROL_SOCKET".to_string(),
            socket_path.to_string_lossy().to_string(),
        ),
    ]
}

/// Prepends `DISCOVERY_INSTRUCTION` to `base` (a handoff summary, or
/// nothing for a brand-new session) — unless `skill_installed`, in which
/// case nothing is prepended at all, trusting the installed skill to
/// trigger on its own. Callers only invoke this for a terminal's actual
/// first-ever launch (`never_launched`); every later relaunch of the same
/// terminal passes `None` through unconditionally instead of calling this
/// at all, since a returning agent already knows and resending would just
/// add a stray synthetic turn to its real conversation.
pub fn discovery_prompt(base: Option<&str>, skill_installed: bool) -> Option<String> {
    let discovery = (!skill_installed).then_some(DISCOVERY_INSTRUCTION);
    match (discovery, base) {
        (None, None) => None,
        (None, Some(base)) => Some(base.to_string()),
        (Some(discovery), None) => Some(discovery.to_string()),
        (Some(discovery), Some(base)) => Some(format!("{discovery}\n\n{base}")),
    }
}

/// Appends `identity`'s orchestration env vars onto `launch` — additive to
/// whatever `agent::with_session_env` already added (`DUET_SESSION_ID`,
/// preserved unchanged for anything already reading it).
pub fn apply(
    mut launch: Launch,
    identity: &AgentIdentity,
    workspace_name: &str,
    socket_path: &Path,
) -> Launch {
    launch
        .envs
        .extend(env_vars(identity, workspace_name, socket_path));
    launch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::role::Role;
    use uuid::Uuid;

    fn identity() -> AgentIdentity {
        AgentIdentity {
            id: Uuid::new_v4(),
            terminal_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            name: "backend".to_string(),
            provider: Agent::Claude,
            role: Some(Role {
                id: Uuid::new_v4(),
                name: "Developer".to_string(),
                instructions: String::new(),
                icon: None,
                accent: None,
                manager: false,
            }),
        }
    }

    #[test]
    fn env_vars_cover_every_required_variable() {
        let identity = identity();
        let vars = env_vars(
            &identity,
            "web",
            Path::new("/run/user/1000/duet/control.sock"),
        );
        let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
        for expected in [
            "DUET_WORKSPACE_ID",
            "DUET_WORKSPACE_NAME",
            "DUET_TERMINAL_ID",
            "DUET_AGENT_ID",
            "DUET_ROLE",
            "DUET_FLOOR_ID",
            "DUET_CONTROL_SOCKET",
        ] {
            assert!(keys.contains(&expected), "missing {expected}");
        }
        assert!(vars.contains(&("DUET_ROLE".to_string(), "Developer".to_string())));
        assert!(vars.contains(&("DUET_FLOOR_ID".to_string(), "ground".to_string())));
    }

    #[test]
    fn no_skill_prepends_discovery_instruction_to_an_existing_base() {
        let combined = discovery_prompt(Some("Continuing from before."), false).unwrap();
        assert!(combined.starts_with(DISCOVERY_INSTRUCTION));
        assert!(combined.ends_with("Continuing from before."));
    }

    #[test]
    fn no_skill_and_no_base_is_just_the_discovery_instruction() {
        assert_eq!(
            discovery_prompt(None, false).as_deref(),
            Some(DISCOVERY_INSTRUCTION)
        );
    }

    #[test]
    fn an_installed_skill_is_trusted_with_no_prompt_at_all() {
        assert_eq!(discovery_prompt(None, true), None);
    }

    #[test]
    fn an_installed_skill_still_passes_a_real_base_through_unprefixed() {
        assert_eq!(
            discovery_prompt(Some("Continuing from before."), true).as_deref(),
            Some("Continuing from before.")
        );
    }
}
