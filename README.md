# duet

A GTK4/libadwaita desktop app for running [`claude`](https://claude.com/claude-code)
and [`codex`](https://github.com/openai/codex) side by side in named,
persistent sessions on an infinite pannable/zoomable canvas — with a way to
hand a conversation off from one agent to the other.

Each session is a card on the canvas with a real PTY running one agent.
Unlike a tabbed UI, every session's terminal is visible at once: drag the
canvas to pan, `Ctrl+scroll` to zoom, and arrange session and sticky-note
cards wherever you like. Session metadata (name, cwd, agent, session id, and
canvas position), sticky notes, and agent-to-agent links are all persisted to
disk on every change and restored when duet reopens.

Each session card has its own link button (drag a link to another session to
forward its output into that session's input — handy for one agent watching
another) and handoff button (switching the agent asks the outgoing agent to
summarize the conversation, then opens the incoming agent with that summary
as its first prompt — a real handoff, not shared context, since Claude and
Codex don't share a session store). A card shows an "exited" badge once its
underlying process exits.

See [`docs/superpowers/specs/2026-09-17-duet-design.md`](docs/superpowers/specs/2026-09-17-duet-design.md)
for the full design.

## Build

Requires the GTK4 desktop stack: `gtk4`, `libadwaita`, and `vte4` (the GTK4
terminal-widget library) as system packages. On Arch:

```sh
sudo pacman -S gtk4 libadwaita vte4
```

(Debian/Ubuntu: `libgtk-4-dev`, `libadwaita-1-dev`, `libvte-2.91-gtk4-dev`.)

```sh
# Install once. This creates `~/.local/bin/duet`, which is on the usual
# Linux desktop PATH.
cargo install --path . --root "$HOME/.local"

# From any project directory, launch duet and it reopens your saved canvas.
duet
```

Requires `claude` and `codex` on `PATH` to actually run agent sessions.

## Workflow

duet opens on a single canvas shared by every session and note — there's no
per-project workspace switch. Sessions you create persist across restarts at
the same canvas position; drag to rearrange, scroll to pan, `Ctrl+scroll` to
zoom.

Accounts (used to keep separate Claude logins/config isolated per session)
are managed through the account-manager dialog rather than ad hoc per-session
prompts: open it, add a name, and it's available to pick when creating a
Claude session.

## Header-bar actions / accelerators

| Button / Accelerator | Action |
|-----------------------|--------|
| New-session button / `Ctrl+T` | Open the new-session dialog (name, working directory, agent) |
| New-note button        | Drop a sticky note at the viewport center |
| Accounts button / `Ctrl+.`   | Open the account manager (list, create, delete accounts) |
| Per-card link button    | Start a link from this session; click another session to complete it |
| Per-card handoff button | Hand this session off to the other agent in place |

Errors (a failed restore, a failed new-session create, account operations)
surface as in-app toasts rather than being printed to the terminal duet was
launched from.

## Limitations

- Codex identity is per-directory, via `codex resume --last`, not per-session
  — switching or creating more than one session in the same directory to
  Codex means they'll resume/summarize the same underlying Codex session.
  Claude sessions don't have this limitation: each one gets its own pinned
  session id.
- Claude sessions resume by their pinned session id. Codex currently exposes
  `resume --last` at this layer, so multiple restored Codex sessions in the
  same directory can resolve to the same latest Codex conversation.
