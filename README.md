# duet

A terminal workspace for running [`claude`](https://claude.com/claude-code)
and [`codex`](https://github.com/openai/codex) side by side in named,
persistent sessions — with a way to hand a conversation off from one agent to
the other.

Each session owns a real PTY running one agent. The left rail keeps every
session visible, while the main pane is the live terminal for the selected
one. Session metadata (name, cwd, agent, and session id) is persisted to disk
on every change and restored when Duet opens. Switching the agent inside a
session asks the outgoing
agent to summarize the conversation, then opens the incoming agent with that
summary as its first prompt — a real handoff, not shared context (Claude and
Codex don't share a session store).

See [`docs/superpowers/specs/2026-09-17-duet-design.md`](docs/superpowers/specs/2026-09-17-duet-design.md)
for the full design.

## Build

```sh
# Install once. This creates `~/.local/bin/duet`, which is on the usual
# Linux desktop PATH.
cargo install --path . --root "$HOME/.local"

# From any project directory, reopen your saved workspace.
duet
```

Requires `claude` and `codex` on `PATH`.

## Keybindings

| Key            | Action              |
|----------------|----------------------|
| `Ctrl+T` / `Ctrl+N` | Open the new-session picker |
| `Ctrl+W`       | Close tab             |
| `Ctrl+Left/Right` | Switch tab         |
| `Ctrl+A`       | Switch agent          |
| `Ctrl+G`       | Switch account         |
| `Ctrl+O`       | Add a Claude account   |
| `Ctrl+R`       | Restart tab            |
| `Ctrl+Q`       | Quit                   |

## Limitations

- Codex identity is per-directory, via `codex resume --last`, not per-tab —
  switching or creating more than one tab in the same directory to Codex
  means they'll resume/summarize the same underlying Codex session. Claude
  tabs don't have this limitation: each tab gets its own pinned session id.
- Claude sessions resume by their pinned session id. Codex currently exposes
  `resume --last` at this layer, so multiple restored Codex tabs in the same
  directory can resolve to the same latest Codex conversation.
