# duet

A GTK4/libadwaita desktop app for running [`claude`](https://claude.com/claude-code)
and [`codex`](https://github.com/openai/codex) side by side in named,
persistent sessions on an infinite pannable/zoomable canvas — with a way to
hand a conversation off from one agent to the other.

Each session is a card on the canvas with a real PTY running one agent, and
sticky notes are cards too. Unlike a tabbed UI, every session's terminal is
visible at once: drag empty canvas space to pan the whole view, `Ctrl+scroll`
to zoom, or drag a card's own title bar to move just that card. Session
metadata (name, cwd, agent, session id, canvas position, and size), sticky
notes, and agent-to-agent links are all persisted to disk on every change and
restored when duet reopens.

Every card's title bar has a close button (removes that card — killing its
process, for a session) and a drag handle (the blank space in the title bar;
dragging it moves the card without panning the canvas underneath it). A small
grip in the card's bottom-right corner resizes it. Session cards additionally
have a link button (drag a link to another session to forward its output into
that session's input — handy for one agent watching another) and a handoff
button (switching the agent asks the outgoing agent to summarize the
conversation, then opens the incoming agent with that summary as its first
prompt — a real handoff, not shared context, since Claude and Codex don't
share a session store) and show an "exited" badge once their underlying
process exits.

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

Requires `claude`/`codex`/`opencode` on `PATH` for those providers; "Shell"
runs `$SHELL`, and "Custom command" runs whatever program you configure when
creating the session.

## Workflow

duet opens on a single canvas shared by every session and note — there's no
per-project workspace switch. Sessions and notes persist across restarts at
their saved canvas position and size. Drag a card's title bar to move just
that card, drag its bottom-right corner to resize it, drag empty canvas space
to pan everything at once, and `Ctrl+scroll` to zoom.

The new-session dialog's agent dropdown picks the provider: Claude, Codex,
OpenCode, a plain Shell, or a Custom command (type the program and any
arguments). Only Claude and Codex support handing a session off to "the
other" agent — the other providers are plain interactive processes with no
equivalent resumable-session/summarize hook, so their handoff button reports
that it's unsupported rather than doing something meaningless.

Accounts (used to keep separate Claude logins/config isolated per session)
are managed through the account-manager dialog rather than ad hoc per-session
prompts: open it, add a name, and pick it from the account dropdown in the
new-session dialog when creating a Claude session. The `default` account is
always available and can't be deleted, since it's the implicit fallback for
any Claude session that doesn't pick another one.

## Header-bar actions / accelerators

| Button / Accelerator | Action |
|-----------------------|--------|
| New-session button / `Ctrl+T` | Open the new-session dialog (name, working directory, agent, Claude account) |
| New-note button        | Drop a sticky note at the viewport center |
| Accounts button / `Ctrl+.`   | Open the account manager (list, create, delete accounts) |
| Per-card title-bar drag handle | Move just this card (doesn't pan the canvas) |
| Per-card bottom-right grip | Resize this card |
| Per-card close button   | Remove this card (kills the process, for a session) |
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
