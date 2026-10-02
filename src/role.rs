//! Reusable agent roles: a name plus instructions (optionally shown with an
//! icon and a visual accent on the session card) that a session can
//! optionally reference. Role instructions reach the launched agent through
//! `agent::LaunchRequest`'s existing `initial_prompt` field — the same
//! provider-agnostic hook `handoff.rs` already uses for a handoff summary —
//! so injecting a role never needs provider-specific code here or in
//! `agent.rs`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Role {
    pub id: Uuid,
    pub name: String,
    pub instructions: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    /// Whether an agent assigned this role may exercise `duetctl`'s
    /// manager-only orchestration actions (create/remove an agent, assign a
    /// role) — see Milestone 3's "manager role" requirement: do not give
    /// recruitment permissions to every agent. `#[serde(default)]` so every
    /// custom role saved before this field existed loads as a non-manager,
    /// never silently gaining recruitment rights it was never granted.
    #[serde(default)]
    pub manager: bool,
}

/// The fixed accent palette a role (built-in or custom) can pick from;
/// `style.css` has one `.role-accent-<name>` rule per entry. Shared with
/// `node.rs` so it can clear every possible accent class before applying
/// whichever one is current.
pub const ACCENTS: &[&str] = &["blue", "purple", "green", "orange", "gray"];

/// Built-in roles, with fixed ids so a session's `role_id` keeps pointing at
/// the same role across restarts. Not persisted (unlike custom roles, kept
/// in `Store::custom_roles`) — always available, the same way
/// `account::DEFAULT_ACCOUNT` always is.
pub fn builtin_roles() -> Vec<Role> {
    vec![
        Role {
            id: Uuid::from_u128(1),
            name: "Developer".to_string(),
            instructions: "You are the Developer. Implement features and fix bugs with \
                working, minimal code; ask only when truly blocked."
                .to_string(),
            icon: Some("utilities-terminal-symbolic".to_string()),
            accent: Some("blue".to_string()),
            manager: false,
        },
        Role {
            id: Uuid::from_u128(2),
            name: "Reviewer".to_string(),
            instructions: "You are the Reviewer. Examine changes for correctness, security, \
                and quality; point out issues precisely with file/line references instead of \
                writing new features."
                .to_string(),
            icon: Some("edit-find-symbolic".to_string()),
            accent: Some("purple".to_string()),
            manager: false,
        },
        Role {
            id: Uuid::from_u128(3),
            name: "Tester".to_string(),
            instructions: "You are the Tester. Write and run tests, hunt for edge cases, and \
                verify behavior rather than implementing new features."
                .to_string(),
            icon: Some("emblem-ok-symbolic".to_string()),
            accent: Some("green".to_string()),
            manager: false,
        },
        Role {
            id: Uuid::from_u128(4),
            name: "Lead".to_string(),
            instructions: "You are the Lead. Make architectural decisions, keep scope tight, \
                and coordinate the work; delegate implementation details rather than writing \
                all the code yourself."
                .to_string(),
            icon: Some("starred-symbolic".to_string()),
            accent: Some("orange".to_string()),
            // The one built-in role with recruiting/coordination permissions
            // — see Milestone 3's manager-role requirement.
            manager: true,
        },
        Role {
            id: Uuid::from_u128(5),
            name: "Documentation".to_string(),
            instructions: "You are the Documentation agent. Write and maintain clear docs, \
                READMEs, and comments explaining why, not just what."
                .to_string(),
            icon: Some("text-x-generic-symbolic".to_string()),
            accent: Some("gray".to_string()),
            manager: false,
        },
    ]
}

/// The fixed ids `builtin_roles()` uses, kept in sync with it by hand (there
/// are only five, and they never change). Lets `is_builtin` check membership
/// without reallocating all five `Role`s — called once per role on every
/// role-manager repopulate.
const BUILTIN_IDS: [Uuid; 5] = [
    Uuid::from_u128(1),
    Uuid::from_u128(2),
    Uuid::from_u128(3),
    Uuid::from_u128(4),
    Uuid::from_u128(5),
];

/// `true` for any id a built-in role uses — keeps the role manager from
/// offering to edit/delete them, the same way `account::DEFAULT_ACCOUNT`
/// can't be deleted from the account manager.
pub fn is_builtin(id: Uuid) -> bool {
    BUILTIN_IDS.contains(&id)
}

/// Combines a role's instructions with whatever prompt text a launch already
/// has (a handoff summary, or nothing for a brand-new session) into one
/// `initial_prompt` — the single place role injection happens, reused by
/// every launch site instead of each one re-deciding how to fold a role in.
/// Goes through the same provider-agnostic `agent::LaunchRequest::initial_prompt`
/// field `handoff.rs` already uses, so it needs no Claude-specific code.
pub fn with_role_instructions(
    role_instructions: Option<String>,
    base: Option<&str>,
) -> Option<String> {
    match (role_instructions, base) {
        (None, None) => None,
        (Some(role), None) => Some(role),
        (None, Some(base)) => Some(base.to_string()),
        (Some(role), Some(base)) => Some(format!("{role}\n\n{base}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_roles_have_stable_unique_ids() {
        let roles = builtin_roles();
        assert_eq!(roles.len(), 5);
        let mut ids: Vec<Uuid> = roles.iter().map(|r| r.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn is_builtin_recognizes_builtins_and_rejects_others() {
        assert!(is_builtin(Uuid::from_u128(1)));
        assert!(!is_builtin(Uuid::new_v4()));
    }

    #[test]
    fn with_role_instructions_combines_role_and_base_when_both_present() {
        assert_eq!(with_role_instructions(None, None), None);
        assert_eq!(
            with_role_instructions(Some("Be terse.".to_string()), None),
            Some("Be terse.".to_string())
        );
        assert_eq!(
            with_role_instructions(None, Some("Summarize this.")),
            Some("Summarize this.".to_string())
        );
        assert_eq!(
            with_role_instructions(Some("Be terse.".to_string()), Some("Summarize this.")),
            Some("Be terse.\n\nSummarize this.".to_string())
        );
    }
}
