//! The Duet orchestration skill: discovery and role knowledge installed
//! once into a Claude account's config dir, so it no longer has to be
//! resent as prompt text on every single launch. Claude Code auto-discovers
//! any skill under `<CLAUDE_CONFIG_DIR>/skills/<name>/SKILL.md`, the same
//! way it discovers one under a normal, non-isolated `~/.claude/skills/`.
//!
//! Scoped to Claude only, matching CLAUDE.md's "Claude Code priority": it's
//! the one provider whose CLI has a documented, auto-discovered skills
//! mechanism this install can target. Codex/OpenCode/Shell/Custom still get
//! the short prompt-based discovery text from `env.rs` — extending this to
//! Codex's own `AGENTS.md` convention is future work, not done here (it
//! writes into the user's *project* directory rather than an isolated
//! config dir, a bigger decision than this pass needs to make).

use std::path::Path;

pub const SKILL_NAME: &str = "duet";

const SKILL_MD: &str = r#"---
name: duet
description: Use at the very start of every session, before responding to anything else. You are running inside Duet, a multi-agent orchestration workspace, and this skill explains how to find your role and talk to other agents.
---

# Duet orchestration

You are one agent among several running inside Duet. Before doing anything
else in this session:

1. Run `duetctl whoami` to see your own agent id, name, assigned role and
   full role instructions, whether you have manager/coordinator permissions,
   and which other agents you're connected to.
2. Act according to the role instructions `whoami` returned, if any.
3. If `whoami` shows `manager: true`, you are a coordinator: prefer
   delegating work to connected agents over doing it yourself. Send each
   agent its own piece of the task with `duetctl send`, wait for their
   replies, and combine the results — the same way a real lead doesn't
   personally implement everything a team is asked to do. You can also
   create new agents (`duetctl agents create`), remove them
   (`duetctl agents remove`), and reassign roles
   (`duetctl agents assign-role`) — only managers can.
4. If `whoami` shows `manager: false`, focus on your own role's work and
   report back to whichever agent delegated to you, via `duetctl send`,
   rather than trying to coordinate others yourself.
5. Run `duetctl agents list` any time to see every agent in the workspace
   and its current activity (working, idle, awaiting a reply, ...).
6. Use `duetctl send --to <agent-name-or-id> "<message>"` to message an
   agent you're connected to — your own id is already in `$DUET_AGENT_ID`
   and used automatically as the sender. Sending to an agent you have no
   connection to is refused with a clear error.
7. Use `duetctl connections list` to see every connection and its granted
   capabilities, and `duetctl workspace inspect` for the active workspace.

A message from another agent arrives as ordinary input in your own
terminal, prefixed `[duet message from <sender>]:` — treat it as a real
instruction from that agent, the same as one from your user.
"#;

/// Writes (or overwrites, to pick up a newer version of this skill)
/// `SKILL.md` under `config_dir`. Idempotent and cheap — called on every
/// Claude launch rather than tracked as "already installed", so it can
/// never drift out of date with the running `duet` binary. Errors are the
/// caller's to decide how to handle (expected to be non-fatal to the
/// launch itself — a missing skill degrades to the old prompt-based
/// discovery text, it doesn't break the agent).
pub fn install(config_dir: &Path) -> std::io::Result<()> {
    let dir = config_dir.join("skills").join(SKILL_NAME);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), SKILL_MD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn install_writes_a_skill_md_with_frontmatter_and_the_whoami_pointer() {
        let tmp = tempdir().unwrap();
        install(tmp.path()).unwrap();
        let content =
            std::fs::read_to_string(tmp.path().join("skills").join(SKILL_NAME).join("SKILL.md"))
                .unwrap();
        assert!(content.starts_with("---\nname: duet"));
        assert!(content.contains("description:"));
        assert!(content.contains("duetctl whoami"));
    }

    #[test]
    fn install_is_idempotent() {
        let tmp = tempdir().unwrap();
        install(tmp.path()).unwrap();
        install(tmp.path()).unwrap(); // must not error on a second call
        assert!(
            tmp.path()
                .join("skills")
                .join(SKILL_NAME)
                .join("SKILL.md")
                .exists()
        );
    }
}
