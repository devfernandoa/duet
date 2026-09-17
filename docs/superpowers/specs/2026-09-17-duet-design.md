# duet — design spec

Date: 2026-09-17

## Background

Started as "adapt Xirp (a macOS-only Electron app that wraps a proprietary
CLI agent binary) to run on Arch Linux." That's not possible: Xirp's actual
engine is a 148MB closed-source Mach-O binary with no Linux build published
anywhere (checked the update CDN directly — no `latest-linux.yml`). Its
Electron shell could be repackaged for Linux, but it would just be a GUI
around a binary the kernel can't execute.

`duet` is a from-scratch tool, not a port: a terminal UI that manages tabbed
sessions of two CLI agents we actually have — `claude` and `codex` — with a
best-effort way to hand a conversation off from one to the other.

## Goals

- Multiple named tabs, each bound to a working directory, each running
  either `claude` or `codex` in a real pty.
- Tabs persist across restarts (a JSON record of name/cwd/session ids), so
  you can relaunch `duet` and resume where you left off, using each CLI's
  own resume mechanism.
- Switch the active agent within a tab without starting over: ask the
  outgoing agent to summarize the conversation (non-interactively), then
  open the incoming agent with that summary as its opening prompt. This is
  a real handoff of a text summary, not shared context — Claude and Codex
  have no shared session store and never will.
- Multiple Claude accounts (e.g. personal vs. work): each Claude tab is
  bound to a named account, isolated via Claude Code's own
  `CLAUDE_CONFIG_DIR` env var (verified empirically — pointing it at an
  empty directory gives Claude a fully self-contained config/credentials/
  projects store). Codex is untouched; this is scoped to Claude only
  because that's the only one asked for.

## Non-goals

- No GUI (explicit user preference: Rust TUI only, no Electron/webview).
- No attempt to run or emulate the original Xirp binary.
- No daemon, no PR-monitoring, no multi-user features — single local user,
  single machine.

## Architecture

One Rust binary. `ratatui` draws a tab bar, the focused tab's live terminal
pane, and a status line. Every open tab keeps its pty and child process
running in the background (not just the focused one), so switching tabs
shows current output immediately, like a small multiplexer scoped to these
two agents.

## Components

- **`tab.rs`** — `Tab`: name, cwd, current `Agent`, its `portable-pty` pair
  + child handle, and a `vt100` screen buffer via the `tui-term` crate
  (reuses Alacritty's terminal emulation — no hand-rolled VT100 parser).
- **`agent.rs`** — builds argv per agent:
  - Claude: `duet` generates a UUID per tab up front and passes
    `--session-id <uuid>` on first launch, `--resume <uuid>` after — Claude
    supports pinning the session id, so our own id is authoritative.
  - Codex: no equivalent flag. On first launch we let Codex assign its own
    id and discover it afterward from its local session store; from then on
    `codex resume <id>`.
- **`handoff.rs`** — the agent-switch flow: drop the outgoing pty, run that
  agent non-interactively (`claude -p --resume <id> "summarize this
  session"` / `codex exec resume <id> "..."`), capture stdout as the
  summary, then spawn the incoming agent with that summary as its initial
  prompt.
- **`store.rs`** — tab records (name, cwd, claude_id, codex_id,
  claude_account) persisted as one JSON file under
  `$XDG_DATA_HOME/duet/tabs.json` (via the `dirs` crate), loaded at startup.
- **`account.rs`** — Claude account isolation. An account is a name mapped
  to `$XDG_DATA_HOME/duet/accounts/<name>/`. The filesystem is the
  registry: listing that directory *is* listing known accounts, no
  separate index to keep in sync. Creating a tab's Claude side with a new
  account name just `mkdir -p`s the directory; `duet` sets
  `CLAUDE_CONFIG_DIR` to it before spawning `claude`, so the normal
  `claude auth login` flow inside that pty populates it — no custom auth
  UI needed. A tab remembers which account it last used for Claude.

## Data flow

Keypress → global keybind (new/close/switch tab, switch agent, quit)
handled by the app; everything else forwarded raw to the focused tab's pty
stdin. Pty stdout bytes → `vt100` parser (inside `tui-term`) → grid state →
`ratatui` redraw.

## Error handling

- Agent binary missing from `PATH` → inline error in that tab's pane, not a
  crash.
- Child process exits unexpectedly → pane shows exit status and a restart
  hint instead of the tab silently closing.
- Corrupt or missing `tabs.json` → start with an empty tab list; never
  hard-fail on startup because of a bad persistence file.

## Testing

Ponytail rule: non-trivial logic gets one runnable check, nothing more.

- `agent.rs`: unit tests for argv construction (fresh vs. resume, both
  agents).
- `store.rs`: unit test for JSON save/load round-trip via a temp file.
- `account.rs`: unit test that the account name → directory mapping and
  listing behave correctly against a temp directory.
- No integration test that spawns a real pty/agent process — too heavy for
  a single-user personal tool, and YAGNI.

## Dependencies

`ratatui`, `crossterm`, `tui-term`, `portable-pty`, `serde`/`serde_json`,
`uuid` (with the `v4` and `serde` features), `dirs`, `anyhow`. All mature,
all on crates.io. Verified the project builds clean on this machine
(Arch Linux, rustc 1.98.1) with this exact dependency set.

## Open questions for implementation

- Exact keybindings (tab new/close/next/prev, agent switch) — pick sensible
  defaults (e.g. `Ctrl+T`/`Ctrl+W`/`Ctrl+Tab`/`Ctrl+A`) during
  implementation, not worth blocking the spec on.
- How `duet` discovers a freshly-created Codex session id (most likely:
  diff Codex's session directory before/after spawn, or parse its startup
  output) — confirm the exact mechanism against the installed Codex CLI
  during implementation.
