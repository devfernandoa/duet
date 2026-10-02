//! Agent-launch environment and discovery prompt — section 8: a launched
//! agent should be able to find and talk to the rest of the workspace
//! through `duetctl` without anything hand-fed to it first.

use super::identity::AgentIdentity;
use crate::agent::Launch;
use crate::model::FloorRef;
use std::path::Path;

/// One short paragraph telling a freshly-launched agent that `duetctl`
/// exists and how to use it — section 8: "Keep injected context compact."
/// Not a tutorial: just enough that the agent knows to look, the same way a
/// `man` page's one-line summary is enough to know whether to read further.
pub const DISCOVERY_INSTRUCTION: &str = "You are running inside Duet, a multi-agent workspace. \
Run `duetctl agents list` to see every agent and whether you're connected to it, and \
`duetctl send --from $DUET_AGENT_ID --to <agent> \"message\"` to message one you're connected \
to. `duetctl workspace inspect` and `duetctl connections list` show the rest of the workspace.";

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

/// Prepends the discovery instruction to whatever prompt text a launch
/// already carries (a role's instructions, a handoff summary, both, or
/// neither) — always present, since an agent needs to learn about `duetctl`
/// on its very first launch regardless of whether it also has a role.
pub fn with_discovery(prompt: Option<String>) -> String {
    match prompt {
        Some(existing) => format!("{DISCOVERY_INSTRUCTION}\n\n{existing}"),
        None => DISCOVERY_INSTRUCTION.to_string(),
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
    fn discovery_instruction_is_prepended_not_replacing_existing_prompt() {
        let combined = with_discovery(Some("You are the Developer.".to_string()));
        assert!(combined.starts_with(DISCOVERY_INSTRUCTION));
        assert!(combined.ends_with("You are the Developer."));
    }

    #[test]
    fn discovery_instruction_stands_alone_with_no_existing_prompt() {
        assert_eq!(with_discovery(None), DISCOVERY_INSTRUCTION);
    }
}
