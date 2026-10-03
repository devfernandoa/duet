//! Note domain service: discovery, permission-checked read/patch, and the
//! pure string transforms behind `App`'s `list_notes`/`read_note`/
//! `replace_note`/`append_note`/`patch_note` (`app.rs`) — Milestone 4's
//! `NoteService`. Pure data/logic, no GTK, same boundary every other
//! `orchestration` module keeps: `App` calls into this, then additionally
//! syncs the live `NoteNode` widget so a service-driven write is immediately
//! visible on the canvas (see `app.rs`'s `set_note_markdown`).
//!
//! Permission checks reuse `permissions::authorize` as-is — a Note is just
//! another node id as far as `EdgeRecord`/`authorize` are concerned, so
//! `ReadNote`/`WriteNote` need no new authorization machinery, only new
//! call sites.

use super::permissions::authorize;
use crate::model::{EdgeCapability, EdgeRecord, NodeRecord};
use uuid::Uuid;

/// A short, human-facing label derived from a note's Markdown source: its
/// first heading if it has one, else its first non-blank line, truncated.
/// Never an identity — `NotePayload` has no name field, and a note's `Uuid`
/// is its only stable identity (CLAUDE.md: "human-readable names are not
/// identities") — this exists purely so `duetctl notes list` shows something
/// more useful than a bare id.
pub fn note_title(markdown: &str) -> String {
    const MAX_CHARS: usize = 60;
    let first_line = markdown
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let text = first_line.trim_start_matches('#').trim();
    if text.is_empty() {
        return "(empty note)".to_string();
    }
    if text.chars().count() > MAX_CHARS {
        let truncated: String = text.chars().take(MAX_CHARS).collect();
        format!("{truncated}…")
    } else {
        text.to_string()
    }
}

/// `nodes`' `Note`-kind entries only, by id — the shared filter every
/// listing/discovery function below starts from.
fn note_ids(nodes: &[NodeRecord]) -> impl Iterator<Item = Uuid> + '_ {
    nodes
        .iter()
        .filter(|node| node.as_note().is_some())
        .map(|node| node.id)
}

/// Every note in `nodes`, unfiltered — what the human operator (`None` in
/// `App::list_notes`) sees: the same unconditional trust `MessageBus::send`
/// already gives a sourceless request.
pub fn all_note_ids(nodes: &[NodeRecord]) -> Vec<Uuid> {
    note_ids(nodes).collect()
}

/// Every note reachable from `agent_id` by one direct edge, regardless of
/// which capability (if any) that edge grants — listing is discovery, not
/// itself capability-gated (`read_note`/`replace_note`/... still enforce
/// `ReadNote`/`WriteNote` separately), so an agent can see it's linked to a
/// note it isn't authorized to open. Deliberately one hop only: this does
/// NOT also return notes connected to *those* notes — Milestone 4 section 7
/// explicitly rules out "do not recursively load an entire Note graph
/// automatically."
pub fn notes_connected_to(nodes: &[NodeRecord], edges: &[EdgeRecord], agent_id: Uuid) -> Vec<Uuid> {
    let notes: std::collections::HashSet<Uuid> = note_ids(nodes).collect();
    edges
        .iter()
        .filter_map(|edge| {
            let other = if edge.source == agent_id {
                edge.target
            } else if edge.target == agent_id {
                edge.source
            } else {
                return None;
            };
            notes.contains(&other).then_some(other)
        })
        .collect()
}

/// Checked only when `actor` is `Some` — an agent-issued request. `None` (a
/// bare CLI invocation or the GUI, with no acting agent identity) is the
/// human operator, trusted unconditionally, the same level `MessageBus::send`
/// and the GUI's own note editing already have.
pub fn authorize_note(
    edges: &[EdgeRecord],
    actor: Option<Uuid>,
    note_id: Uuid,
    capability: EdgeCapability,
) -> Result<(), String> {
    match actor {
        Some(actor) => authorize(edges, actor, note_id, capability),
        None => Ok(()),
    }
}

/// Appends `addition` to `current`, inserting a separating newline only if
/// `current` is non-empty and doesn't already end with one — never two
/// blank lines, never none at all.
pub fn apply_append(current: &str, addition: &str) -> String {
    if current.is_empty() || current.ends_with('\n') {
        format!("{current}{addition}")
    } else {
        format!("{current}\n{addition}")
    }
}

/// Replaces the one occurrence of `old` in `current` with `new`. Fails if
/// `old` doesn't occur, or occurs more than once (ambiguous which to
/// replace) — the same "exact, unique match" contract this project's own
/// editing tools use. This doubles as Milestone 4's concurrency check for
/// patch operations: if another agent's edit already changed the text `old`
/// names, this fails cleanly (the match is simply gone or shifted) instead
/// of silently clobbering that edit — "validate that the intended
/// base/current content still matches" without needing a separate revision
/// counter.
pub fn apply_patch(current: &str, old: &str, new: &str) -> Result<String, String> {
    if old.is_empty() {
        return Err("patch needs non-empty text to match against the note".to_string());
    }
    match current.matches(old).count() {
        0 => Err("old text not found in the note's current content".to_string()),
        1 => Ok(current.replacen(old, new, 1)),
        occurrences => Err(format!(
            "old text occurs {occurrences} times in the note; patch requires exactly one match"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EnvironmentKind, FloorRef, NodeKind, NotePayload, NoteViewMode};

    fn note(markdown: &str) -> NodeRecord {
        NodeRecord {
            id: Uuid::new_v4(),
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (1.0, 1.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Note(NotePayload {
                markdown: markdown.to_string(),
                color: "yellow".to_string(),
                view_mode: NoteViewMode::Preview,
                file: None,
            }),
        }
    }

    fn terminal(id: Uuid) -> NodeRecord {
        NodeRecord {
            id,
            floor: FloorRef::Ground,
            position: (0.0, 0.0),
            size: (1.0, 1.0),
            z_order: 0,
            collapsed: false,
            locked: false,
            kind: NodeKind::Terminal(crate::model::TerminalPayload {
                name: "lead".to_string(),
                cwd: std::path::PathBuf::from("/"),
                agent: crate::agent::Agent::Claude,
                claude_session_id: None,
                claude_account: None,
                never_launched: false,
                role_id: None,
                environment: EnvironmentKind::LocalPty,
            }),
        }
    }

    fn edge(source: Uuid, target: Uuid, capabilities: &[EdgeCapability]) -> EdgeRecord {
        EdgeRecord {
            id: Uuid::new_v4(),
            source,
            target,
            capabilities: capabilities.iter().copied().collect(),
        }
    }

    #[test]
    fn note_title_prefers_the_first_heading() {
        assert_eq!(note_title("# Requirements\n\nbody text"), "Requirements");
    }

    #[test]
    fn note_title_falls_back_to_the_first_non_blank_line() {
        assert_eq!(note_title("\n\nhello there\nmore"), "hello there");
    }

    #[test]
    fn note_title_of_empty_markdown_is_a_placeholder() {
        assert_eq!(note_title(""), "(empty note)");
        assert_eq!(note_title("   \n   "), "(empty note)");
    }

    #[test]
    fn note_title_truncates_long_first_lines() {
        let long = "x".repeat(100);
        let title = note_title(&long);
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= 61);
    }

    #[test]
    fn notes_connected_to_finds_only_one_hop_notes_for_the_given_agent() {
        let agent_id = Uuid::new_v4();
        let direct_note = note("# requirements");
        let far_note = note("# far away, connected only to direct_note");
        let nodes = vec![terminal(agent_id), direct_note.clone(), far_note.clone()];
        let edges = vec![
            edge(agent_id, direct_note.id, &[EdgeCapability::ReadNote]),
            // direct_note -> far_note: a Note-Note edge, never traversed.
            edge(direct_note.id, far_note.id, &[]),
        ];
        let connected = notes_connected_to(&nodes, &edges, agent_id);
        assert_eq!(connected, vec![direct_note.id]);
    }

    #[test]
    fn notes_connected_to_ignores_unrelated_edges_and_non_note_neighbors() {
        let agent_id = Uuid::new_v4();
        let other_agent = Uuid::new_v4();
        let unrelated_note = note("unrelated");
        let nodes = vec![terminal(agent_id), terminal(other_agent), unrelated_note];
        let edges = vec![edge(agent_id, other_agent, &[EdgeCapability::SendMessages])];
        assert!(notes_connected_to(&nodes, &edges, agent_id).is_empty());
    }

    #[test]
    fn human_operator_bypasses_note_authorization() {
        let note_id = Uuid::new_v4();
        assert!(authorize_note(&[], None, note_id, EdgeCapability::ReadNote).is_ok());
    }

    #[test]
    fn agent_without_a_read_note_edge_is_unauthorized() {
        let (agent_id, note_id) = (Uuid::new_v4(), Uuid::new_v4());
        let err =
            authorize_note(&[], Some(agent_id), note_id, EdgeCapability::ReadNote).unwrap_err();
        assert!(err.contains("not authorized"));
    }

    #[test]
    fn read_note_capability_does_not_imply_write_note() {
        let (agent_id, note_id) = (Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![edge(agent_id, note_id, &[EdgeCapability::ReadNote])];
        assert!(authorize_note(&edges, Some(agent_id), note_id, EdgeCapability::ReadNote).is_ok());
        assert!(
            authorize_note(&edges, Some(agent_id), note_id, EdgeCapability::WriteNote).is_err()
        );
    }

    #[test]
    fn write_note_capability_grants_write_access() {
        let (agent_id, note_id) = (Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![edge(agent_id, note_id, &[EdgeCapability::WriteNote])];
        assert!(authorize_note(&edges, Some(agent_id), note_id, EdgeCapability::WriteNote).is_ok());
    }

    #[test]
    fn apply_append_separates_with_exactly_one_newline() {
        assert_eq!(apply_append("line one", "line two"), "line one\nline two");
        assert_eq!(apply_append("line one\n", "line two"), "line one\nline two");
        assert_eq!(apply_append("", "first line"), "first line");
    }

    #[test]
    fn apply_patch_replaces_the_single_match() {
        let updated = apply_patch("# Status\n\nnot started", "not started", "done").unwrap();
        assert_eq!(updated, "# Status\n\ndone");
    }

    #[test]
    fn apply_patch_fails_when_old_text_is_missing() {
        let err = apply_patch("# Status\n\ndone", "not started", "done").unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn apply_patch_fails_when_old_text_is_ambiguous() {
        let err = apply_patch("todo\ntodo", "todo", "done").unwrap_err();
        assert!(err.contains("2 times"));
    }

    #[test]
    fn apply_patch_rejects_empty_old_text() {
        assert!(apply_patch("anything", "", "new").is_err());
    }
}
