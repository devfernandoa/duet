//! The Duet orchestration skill: discovery and role knowledge installed
//! once into an agent's own config dir, so it no longer has to be resent as
//! prompt text on every single launch. Both Claude Code and Codex
//! auto-discover any skill at the identical `skills/<name>/SKILL.md`
//! convention (frontmatter `name`/`description` plus a Markdown body),
//! just rooted at a different directory per provider — `CLAUDE_CONFIG_DIR`
//! for Claude, `CODEX_HOME` for Codex (confirmed against a real `codex`
//! install: `~/.codex/skills/.system/*/SKILL.md` uses this exact shape).
//! `install` itself doesn't know or care which — it just writes the file
//! under whatever config dir its caller resolved.
//!
//! OpenCode/Shell/Custom still get the short prompt-based discovery text
//! from `env.rs` instead — none of them has a documented, auto-discovered
//! skills mechanism this install can target.

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

## Notes

Notes are Markdown documents on the canvas — requirements, specs, status
reports — that you can read and, if connected with write access, update
directly, instead of having their content pasted into your prompt.

1. Run `duetctl notes list` to see every note you're connected to (an empty
   list means you have no note connections yet — ask whoever set up this
   workspace to connect one). This never dumps note content, only ids and
   titles — fetch a note's content only when you actually need it.
2. Run `duetctl notes read <id>` to read one note's full Markdown source.
3. Run `duetctl notes connections <id>` to see what else that note is
   connected to (other agents, other notes) and with what capabilities.
4. To update a note you have write access to:
   - `duetctl notes replace <id>` overwrites the whole note — pipe the new
     Markdown in on stdin, e.g. with a heredoc:
     `duetctl notes replace <id> <<'EOF'` ... Markdown content ... `EOF`.
   - `duetctl notes append <id>` adds text after the note's current
     content — pipe the addition in the same way.
   - `duetctl notes patch <id> --old "<exact text>" --new "<replacement>"`
     changes just one exact, unique piece of text in place — prefer this
     over `replace` for a small edit; it fails cleanly (instead of
     overwriting someone else's change) if `--old` no longer matches the
     note's current content.
5. Reading or writing a note you have no connection to, or no write
   capability on, is refused with a clear error — this is enforced by Duet
   itself, not by this skill, so don't attempt to work around it.
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
        assert!(content.contains("duetctl notes list"));
        assert!(content.contains("duetctl notes read"));
        assert!(content.contains("duetctl notes patch"));
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
