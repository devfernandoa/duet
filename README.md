# duet

A GTK4/libadwaita desktop app for running multiple AI coding agents side by
side on an infinite, pannable/zoomable canvas — and having them talk to each
other. Each agent (Claude Code, Codex, OpenCode, a plain shell, or a custom
command) runs in a real PTY inside its own card; notes, plain-text labels,
and a handful of placeholder node kinds share the same canvas. Connect two
agent cards and they can message each other with `duetctl`, duet's local
control CLI — the same service layer the GUI itself calls, so nothing an
agent can do is a GUI-only trick.

Milestones 0 through 3 (stabilization, the generic canvas, persistent
workspaces, and agent orchestration) are done; floors, scores, the file tree,
browser portals, and the prompt composer are not yet — see
[Known limitations](#known-limitations) below for the current gaps.

## Build

Requires the GTK4 desktop stack: `gtk4`, `libadwaita`, `vte4` (the GTK4
terminal-widget library) and `gtksourceview5` (the embedded editor) as
system packages, plus `git` (and optionally `ripgrep`, for faster content
search) at runtime. On Arch:

```sh
sudo pacman -S gtk4 libadwaita vte4 gtksourceview5
```

(Debian/Ubuntu: `libgtk-4-dev`, `libadwaita-1-dev`, `libvte-2.91-gtk4-dev`,
`libgtksourceview-5-dev`.)

```sh
# Installs both binaries this crate builds: `duet` (the GUI) and `duetctl`
# (its control CLI) — both end up on the usual Linux desktop PATH.
cargo install --path . --root "$HOME/.local"

# From any directory, launch duet and it reopens your last workspace.
duet
```

Requires `claude`/`codex`/`opencode` on `PATH` for those providers; "Shell"
runs `$SHELL`, and "Custom command" runs whatever program you configure when
creating the session.

## Workspaces

Everything lives inside a *workspace*: its own canvas, its own nodes and
edges, its own default working directory and runtime environment. Switch
between them with the workspace button in the header bar or `Ctrl+1`
through `Ctrl+9` (the Nth workspace in name order). Switching away from a
workspace detaches its widgets but never kills its processes — an agent kept
running in the background stays running, and the workspace switcher shows
which ones still have live processes with an "unload" action if you actually
want to stop them. Deleting a workspace unloads it first, so it can never
leak a process nothing references anymore.

Each workspace also picks a default runtime for new terminals: a plain local
PTY, or `tmux`-backed (`duet-<terminal-id>` sessions), which keeps the real
process alive inside tmux's own server across a `duet` restart rather than as
a direct child of it.

## The canvas

Every object on the canvas — a terminal, a note, a plain-text label — is a
persisted node with a stable id, position, size, z-order, collapsed/locked
state, and (for terminals) an assigned role. `Portal`, `Drawing`, and
`Group` exist as placeholder node kinds today (creatable from the edit
menu's Create section) ahead of the milestones that give them real behavior.

Every card works the same way:

- **Click anywhere on it** to select it and bring it to the front
  (Shift/Ctrl-click adds to the selection; Shift-drag empty canvas for a
  marquee).
- **Drag its title bar** — the name included — to move it (dragging one of
  several selected cards moves them all); drag its bottom-right corner to
  resize it.
- Its title bar only shows its name, status, **collapse** (`⌃`) and
  **close**. **Right-click the title bar** for everything else: rename,
  restart, Edit/Preview, save, find, Git actions, "Connect to another
  card…", lock, duplicate, delete.

**Right-click empty canvas** to create a terminal, note, text or file tree
right where you clicked.

The mouse wheel over empty canvas zooms around the pointer (`Ctrl+wheel`
zooms over cards too); on a touchpad, two-finger scroll pans and pinch
zooms. Drag empty canvas to pan. (The zoom buttons and
`Ctrl +`/`Ctrl -`/`Ctrl 0` work too.) The edit-menu button (no keyboard accelerator of its own, since a
focused terminal needs `Ctrl+Z`/`Ctrl+C`/`Ctrl+A`/`Delete` unshadowed) holds
selection, layout (align/distribute), duplicate/copy/paste, undo/redo,
front/back ordering, lock/collapse, snap-to-grid, and node creation.

### Terminals

The new-session dialog's agent dropdown picks the provider: **Claude**,
**Codex**, **OpenCode**, a plain **Shell**, or a **Custom command** (type the
program and any arguments). The same dialog assigns a role (see
[Roles](#roles-and-connections) below) and, for Claude, picks which isolated
account to use.

Claude and Codex sessions can be handed off from the card's menu (**Hand off
to another agent…**): pick Claude on any of your accounts, or Codex. Duet
summarizes the outgoing conversation in the background and starts the
chosen agent in the same card with that summary as its first prompt — the
way to move a session to another account, too. OpenCode, Shell and custom
commands have no resumable conversation to summarize, so their menu doesn't
offer a handoff.

A terminal's title bar shows its name (double-click to rename), role, and
current activity (`starting`, `idle`, `working`, `awaiting reply`, `offline`,
`finished`, `failed`) once it's knowable — nothing is guessed when it isn't.

### Notes

Note cards edit and render Markdown: headings, lists, task lists, links,
fenced and inline code, blockquotes, and tables (parsed, shown as plain rows
rather than aligned cells). Double-click a rendered note to edit it; switch
between Edit, Preview, and Edit + preview from the note's menu. Plain-text source editing is always available in Edit
mode — a Note is just Markdown source plus a render of it, never a separate
representation that can drift from what you typed.

## Roles and connections

A terminal can be assigned a role — **Developer**, **Reviewer**, **Tester**,
**Lead**, **Documentation**, or any custom role you create — through the
new-session dialog. A role carries instructions text, an optional icon, and
an accent color shown as a badge on the card's title bar. Manage custom
roles through the roles button or `Ctrl+Shift+R`; built-in roles can't be
edited or deleted. **Lead** is the one built-in *manager* role: an agent
assigned it can recruit and dismiss other agents and reassign roles (see
`duetctl agents create/remove/assign-role` below) — every other role can't,
so recruiting permissions aren't handed out by default.

To connect two cards, pick **Connect to another card…** from one card's
menu, then click the other card (`Esc` cancels). What a connection grants
depends on what it joins, since there's no capability editor yet:

- terminal ↔ terminal: `SendMessages` (the agents can message each other);
- terminal ↔ note: `ReadNote` + `WriteNote`;
- terminal ↔ editor or file tree: `ShareContext` — the file or folder shows
  up in that agent's `duetctl whoami` as context to start from;
- anything else: a purely visual connection.

Lines are drawn under the cards. Click a line to select it and click it
again to delete it, or use **Remove connections** in a card's menu.

## Agent orchestration (`duetctl`)

Agents talk to each other and to duet itself through `duetctl`, a thin CLI
over duet's local Unix-domain control socket — the exact same application
services the GUI calls, so nothing is reimplemented twice. It only works
while `duet` is running.

```
duetctl agents list                                    # every agent, its role, and current activity
duetctl agents inspect <id-or-name>                     # one agent's full detail
duetctl agents create --name <n> --cwd <dir> --agent <claude|codex|opencode|shell> [--role <role>]
duetctl agents remove <id-or-name>                      # manager-role agents only
duetctl agents assign-role <id-or-name> <role-or-none>  # manager-role agents only
duetctl send --from <id> --to <id-or-name> "<message>"  # refused without a SendMessages connection
duetctl connections list                                # every edge and its capabilities
duetctl workspace inspect                                # the active workspace's own metadata
duetctl whoami                                           # an agent's own identity, role, and connections
```

Run from a bare shell (no `DUET_AGENT_ID` in the environment), `duetctl`
acts with the same trust the GUI already has. Run from inside a
duet-launched agent's own shell, it acts as that agent: `send`/`whoami`
resolve "who's asking" from `$DUET_AGENT_ID` automatically, and manager-only
actions are refused with a clear error for anything but a manager-role
agent. `duet agent list` / `duet agent send <target> "..."` still work too,
preserved for anything already using them.

### Project files and Git (`duetctl file`, `duetctl git`)

Every workspace's root directory is its *project*. Files in it are Duet
resources: `@file:src/auth.rs` (or `@file:src/auth.rs#L10-20` for a line
range) and `@diff:src/auth.rs` (uncommitted changes; `@diff:.` for
everything) resolve through `duetctl resolve` like any other reference, and
can never point outside the project root.

```sh
duetctl file inspect <path|@file:path>             # size, revision, Git status
duetctl file read <path|@file:path#L1-20>          # content (or a line range)
duetctl file list [dir] [--hidden]
duetctl file search <query>                        # fuzzy file names
duetctl file search --content <pattern>            # ripgrep (builtin fallback)
duetctl file write <path> --revision <rev>         # stdin; refused if the file changed since <rev>
duetctl git status | diff [path] [--staged|--unstaged] | log [path] [-n N]
duetctl git stage <path>... | unstage <path>... | discard <path>... --confirm | commit -m "<msg>"
duetctl notes attach <note-id> <path>              # sync a note with a Markdown file
```

On the canvas, a **File Tree** node (header folder button, or Edit menu →
New File Tree) browses the project with Git status markers, hidden-file and
`.gitignore` toggles, back/forward, fuzzy search, and `>pattern` content
search; right-click a file to open it, diff/stage/unstage/discard it, copy
its `@file:` reference, or ask an agent about it. Markdown files open as
file-backed notes that follow external edits and never overwrite a
concurrent change silently (a conflict banner asks instead); other text
opens in a GtkSourceView editor (Ctrl+S save, Ctrl+F/H find/replace, Ctrl+G
go to line). Dragging a file from the tree — or from a file manager — onto
the canvas opens it there.

A message delivered to an agent is queued, delivered in order (never two
messages interleaved mid-delivery to the same agent), and arrives as
ordinary terminal input prefixed `[duet message from <sender>]:` — the
receiving agent reads it the same way it would read something its own user
typed.

### What a launched agent already knows

Every terminal gets `DUET_WORKSPACE_ID`, `DUET_WORKSPACE_NAME`,
`DUET_TERMINAL_ID`, `DUET_AGENT_ID`, `DUET_ROLE`, `DUET_FLOOR_ID`, and
`DUET_CONTROL_SOCKET` in its environment. Claude and Codex both also get a
`duet` skill installed into their own config directory on every launch
(`skills/duet/SKILL.md` — Claude and Codex use the identical convention),
telling them to run `duetctl whoami` before doing anything else to learn
their role and connections, and — for a manager-role agent specifically —
that they should delegate to connected agents rather than do the work
themselves. Nothing is resent as a prompt on a terminal's second or later
launch (restore, reattach, restart): only its actual first-ever launch
sends anything at all, so returning to a workspace never pastes a stray
synthetic message into an agent's real conversation. OpenCode, Shell, and
Custom commands have no comparable skill mechanism and get a short one-time
prompt instead, on that same first-launch-only basis.

## Claude accounts

Accounts keep separate Claude logins/config isolated per terminal. Manage
them through the account-manager dialog (`Ctrl+.`): add a name, then pick it
from the account dropdown when creating a Claude session. `default` is
always available and can't be deleted, since it's the implicit fallback for
any Claude terminal that doesn't pick another one. Several terminals can
safely share one account and run concurrently.

## Codex specifics

Codex works differently from Claude in two ways duet works around:

- Its local app-server daemon tolerates only one live interactive session
  per `CODEX_HOME`, so every Codex terminal gets its own, isolated
  `CODEX_HOME` (keyed by the terminal's own id) rather than sharing one —
  unlike Claude, where several concurrent terminals safely share a named
  account. That isolation would normally cost a separate `codex login` per
  terminal; duet avoids that by symlinking each terminal's `auth.json` to
  one shared location instead, so logging in from any one Codex terminal
  authenticates every other one too.
- Codex's own built-in default is a `read-only` sandbox with approval
  prompts for most commands — fine for a human watching an interactive
  session, but it silently blocks an unattended agent from writing files or
  reaching `duetctl` at all. Every Codex launch instead explicitly passes
  `workspace-write` with network access enabled and `--ask-for-approval
  never`: a command that would have needed approval simply fails and is
  reported back to the model, the same as any other command failure it
  already has to handle.

Because identity is isolated per terminal now, two Codex terminals in the
same working directory no longer resume/summarize the same underlying Codex
conversation the way they used to.

## Header-bar actions / accelerators

| Button / Accelerator | Action |
|-----------------------|--------|
| New-session button / `Ctrl+T` | Open the new-session dialog (name, working directory, agent, role, Claude account) |
| New-note button | Drop a Markdown note at the viewport center |
| Accounts button / `Ctrl+.` | Open the account manager (list, create, delete Claude accounts) |
| Roles button / `Ctrl+Shift+R` | Open the role manager (list built-ins, create/edit/delete custom roles) |
| Edit-menu button | Selection, layout (align/distribute), duplicate/copy/paste, undo/redo, front/back, lock/collapse, snap-to-grid, node creation — no accelerator (a focused terminal needs `Ctrl+Z`/`Ctrl+C`/`Ctrl+A`/`Delete` unshadowed) |
| `Ctrl +` / `Ctrl -` / `Ctrl 0` | Zoom in / out / reset |
| Workspace button / `Ctrl+1`..`Ctrl+9` | Switch workspace (via dialog, or jump straight to the Nth) |
| Per-card title-bar drag handle | Move just this card (doesn't pan the canvas) |
| Per-card bottom-right grip | Resize this card |
| Per-card close button | Remove this card (kills the process, for a terminal) |
| Right-click a card's title bar | The card's menu (rename, connect, collapse, lock, duplicate, delete, ...) |
| Card menu → Connect to another card… | Then click the card to connect to; `Esc` or a click on empty canvas cancels |
| Right-click empty canvas | New terminal / note / text / file tree at that spot |
| Wheel on empty canvas, `Ctrl+wheel` anywhere, pinch | Zoom around the pointer |
| Two-finger scroll, drag on empty canvas | Pan |

Errors (a failed restore, a failed create, account/role operations) surface
as in-app toasts rather than being printed to the terminal duet was launched
from.

## Data locations

- Workspaces, roles, and schema version: `$XDG_DATA_HOME/duet/store.json`
  (atomic write: temp file, fsync, rename).
- Claude accounts: `$XDG_DATA_HOME/duet/accounts/<name>/` (one
  `CLAUDE_CONFIG_DIR` each).
- Codex per-terminal isolation: `$XDG_DATA_HOME/duet/codex/<terminal-id>/`
  (one `CODEX_HOME` each); shared login: `$XDG_DATA_HOME/duet/codex-auth/default/auth.json`.
- Control socket: `$XDG_RUNTIME_DIR/duet/control.sock` (falls back to the
  data dir if no runtime dir is available).

`$XDG_DATA_HOME` defaults to `~/.local/share` and `$XDG_RUNTIME_DIR` to
`/run/user/<uid>` when unset, per the usual XDG conventions.

## Known limitations

- `Portal`, `Drawing`, and `Group` are placeholder node kinds only — no
  embedded browser, freehand drawing, or containment semantics yet.
- Projects are local only: `ProjectFilesystem` has a single, local
  implementation until SSH/Docker environments arrive. The editor has no
  tabs or split view.
- `ReadNote`, `WriteNote`, `ControlPortal`, and `ShareContext` edge
  capabilities are representable but not enforced by anything yet, since
  the features they'd gate don't exist.
- No prompt composer or `@`-mention resolution yet — messaging is
  `duetctl send` only.
- No floors (git-isolated parallel work), reusable arrangements ("Scores"),
  or cross-workspace search yet.

The original design rationale (predating the orchestration work above) is in
[`docs/superpowers/specs/`](docs/superpowers/specs/).
