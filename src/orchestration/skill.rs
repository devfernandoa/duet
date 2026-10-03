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
description: Use at the very start of every session, before responding to anything else. You are running inside Duet, a multi-agent orchestration workspace, and this skill explains how to find your role, talk to other agents, and use Duet resources — notes, project files and browser portals — including @references such as @portal:frontend.
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

## Resource references (@)

When an instruction contains an explicit `@` reference — `@backend`,
`@agent:backend`, `@note:requirements` — treat it as a Duet resource
reference, not an ordinary word, and resolve it through Duet rather than
guessing:

1. Run `duetctl resolve @backend` (add `--json` for machine-readable output).
   Never invent a resource's id or assume its identity from the reference
   text — Duet is the only source of truth for what a reference names.
2. If the result reports more than one match, use a qualified form
   (`@agent:backend`, `@note:backend`) to disambiguate, or look at the
   workspace/floor Duet returned for each candidate and pick accordingly.
3. Act on the resolved id with the command for its kind: `duetctl send --to
   <id>` for an agent, `duetctl notes read/replace/append/patch <id>` for a
   note. `duetctl resource inspect <id>` shows basic metadata for any kind.
4. A prompt may name several resources at once (e.g. "ask @backend to read
   @requirements and send the result to @reviewer") — resolve each
   reference independently, then carry out each step with its own command.
   Duet does not parse or orchestrate the sentence itself; you do.

You may also recognize an Agent or Note described in plain words (without
`@`) by inspecting `duetctl agents list` / `duetctl notes list` — but an
explicit `@reference` is the deterministic signal and always takes priority
over a guess from plain language.

## Project files (@file:, @diff:)

`@file:<path>` names a file in *your* workspace's project root (Duet scopes it
for you — never guess which checkout or workspace a path belongs to).
`@file:src/auth.rs#L10-20` names lines 10-20 of it. `@diff:<path>` names that
path's uncommitted Git changes (`@diff:.` for the whole project). A
path-shaped reference without a qualifier, like `@src/auth.rs` or
`@README.md`, may also mean a file.

1. Resolve the reference first: `duetctl resolve @file:src/auth.rs`. If an
   unqualified `@name.md` is ambiguous (a note and a file), use the
   qualified form. Paths outside the project root are refused by design.
2. Then use the file-specific command:
   - `duetctl file read <path>` (or the full `@file:...#L10-20` reference;
     `--lines 10-20` also works) prints the content;
     `duetctl file inspect <path>` shows size, revision and Git status.
   - `duetctl file search <query>` finds files by fuzzy name;
     `duetctl file search --content <pattern>` searches inside files.
   - `duetctl git diff <path|@diff:path>` shows uncommitted changes
     (`--staged`/`--unstaged` to narrow), `duetctl git status`, `git log`.
3. To change a file through Duet, read it first (`file read --json` or
   `file inspect` gives its `revision`), then
   `duetctl file write <path> --revision <rev>` with the new content on
   stdin (`--create` for a new file). A write based on a stale revision is
   refused — re-read and retry instead of overwriting someone else's change.
   Your normal editing tools are fine too; Duet notices changes either way.
4. When delegating or asking for review, pass the reference
   (`@file:src/auth.rs#L10-20`, `@diff:src/auth.rs`), not pasted content —
   the receiving agent resolves it itself.
5. `duetctl notes attach <note-id> <path>` syncs a note with a Markdown file.
6. `duetctl whoami` lists the files and folders the user connected to you on
   the canvas ("connected files") — start from those when a task is vague
   about which code it means.

## Browser portals (@portal:)

A Portal is an embedded browser on the canvas — the app you're building,
docs, an admin page — that you can drive through Duet. `@portal:frontend`
names a portal by its name; an unqualified `@frontend` may mean a portal too.
Duet owns the browser: use only the `duetctl portal` commands below, never
your own browser automation, and never assume what a page shows.

1. Resolve first: `duetctl resolve @portal:frontend` (or `@frontend`; if it is
   ambiguous, use the qualified form or the portal id it lists).
   `duetctl portal list` shows the portals you're connected to and whether
   you may control them. Every `portal` command accepts the portal's id or
   the reference itself, e.g. `duetctl portal text @portal:frontend`.
2. Inspect before acting: `duetctl portal inspect <portal>` (URL, title,
   loading, history), or just `portal url` / `portal title`.
3. Read the page as text: `duetctl portal text <portal>` returns the page's
   readable text; `--selector "<css>"` narrows it to one element, `--html`
   returns that element's HTML (the DOM) instead. Prefer text — a heading,
   an error message or a form's state doesn't need a screenshot.
4. Take a screenshot only when you need to see layout or visuals:
   `duetctl portal screenshot <portal>` (`--full` for the whole page) prints
   the path of a PNG Duet saved; open that file with your image-reading tool.
5. Navigate where you're authorized: `duetctl portal navigate <portal> <url>`
   (e.g. `localhost:3000`; only http/https), `portal back`, `portal forward`,
   and `portal reload` after you change the code it serves. Each waits for
   the page to finish loading.
6. Interact through Duet only: `duetctl portal click <portal> "<css>"` and
   `duetctl portal type <portal> "<css>" "<text>"` (replaces the field's value;
   `--append` to add, `--submit` to submit its form). If an interaction
   navigates, the command waits for the new page; then read it again.
7. `duetctl portal evaluate <portal> "<js>"` runs arbitrary JavaScript and is
   privileged: it is refused unless the user enabled scripts for that portal.
   Don't ask for it when text/click/type are enough.
8. Controlling a portal needs a connection to it on the canvas; being in the
   same workspace isn't enough. A refusal is enforced by Duet — ask the user
   to connect you rather than working around it.

A typical loop: start the dev server in your terminal, `portal navigate` to
it, `portal text` to check, `click`/`type` to exercise the UI, edit the
source, `portal reload`, `portal text` again to verify.
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
        assert!(content.contains("duetctl resolve"));
        assert!(content.contains("duetctl resource inspect"));
        assert!(content.contains("@agent:backend"));
        assert!(content.contains("@file:src/auth.rs#L10-20"));
        assert!(content.contains("duetctl file read"));
        assert!(content.contains("duetctl git diff"));
        assert!(content.contains("--revision"));
        assert!(content.contains("@portal:frontend"));
        assert!(content.contains("duetctl portal text"));
        assert!(content.contains("duetctl portal screenshot"));
        assert!(content.contains("duetctl portal click"));
        assert!(content.contains("duetctl portal type"));
        assert!(content.contains("portal reload"));
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
