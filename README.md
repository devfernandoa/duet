# duet

A terminal UI for running [`claude`](https://claude.com/claude-code) and
[`codex`](https://github.com/openai/codex) side by side in named, persistent
tabs — with a way to hand a conversation off from one agent to the other.

Each tab owns a real pty running one agent. Tab metadata (name, cwd, agent,
session id) is persisted to disk on every change, but restoring those tabs
into live processes on the next launch isn't implemented yet — each run
starts with no tabs open. Switching the agent inside a tab asks the outgoing
agent to summarize the conversation, then opens the incoming agent with that
summary as its first prompt — a real handoff, not shared context (Claude and
Codex don't share a session store).

See [`docs/superpowers/specs/2026-09-17-duet-design.md`](docs/superpowers/specs/2026-09-17-duet-design.md)
for the full design.

## Build

```sh
cargo build --release
```

## Run

```sh
cargo run
```

Requires `claude` and `codex` on `PATH`.

## Keybindings

| Key            | Action              |
|----------------|----------------------|
| `Ctrl+T`       | New Claude tab       |
| `Ctrl+N`       | New Codex tab        |
| `Ctrl+W`       | Close tab             |
| `Ctrl+Left/Right` | Switch tab         |
| `Ctrl+A`       | Switch agent          |
| `Ctrl+G`       | Switch account         |
| `Ctrl+R`       | Restart tab            |
| `Ctrl+Q`       | Quit                   |

## Limitations

- Codex identity is per-directory, via `codex resume --last`, not per-tab —
  switching or creating more than one tab in the same directory to Codex
  means they'll resume/summarize the same underlying Codex session. Claude
  tabs don't have this limitation: each tab gets its own pinned session id.
- Tab metadata persists to disk (`tabs.json`) but isn't yet restored into
  live tabs on the next launch — see above.
