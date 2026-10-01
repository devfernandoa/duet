# duet — canvas GUI redesign

Date: 2026-10-01

## Background

The original design (`2026-09-17-duet-design.md`) explicitly chose "no GUI."
That preference has changed: the goal now is a Linux/Wayland-native clone of
[Maestri](https://www.themaestri.app/), a macOS app that arranges AI coding
agent terminals spatially on an infinite pan/zoom canvas instead of tabs,
with saved layouts, agent-to-agent terminal linking, and canvas sticky notes.

Maestri also ships features with no Linux equivalent: iOS/Android device
portal embeds, APFS copy-on-write workspace snapshots ("Floors"), and an
Apple Foundation Models local AI companion ("Ombro"). Those are out of scope
for this redesign — see Non-goals.

This supersedes the TUI architecture in the September spec but keeps its
non-UI decisions (session persistence shape, Claude account isolation via
`CLAUDE_CONFIG_DIR`, text-summary handoff) intact.

## Goals (v1)

- Native Wayland desktop app (GTK4 + libadwaita), not Electron/webview.
- An infinite pan/zoom canvas; each agent session is a draggable node
  containing a real terminal.
- Canvas layout (node positions, sizes, zoom/pan, links, notes) persists
  across restarts alongside the existing session data (cwd, agent, Claude
  account, resume ids).
- Agent-to-agent terminal linking: one session's output can feed directly
  into another session's input, so one agent can delegate to another without
  the user relaying text by hand.
- Sticky notes: freeform Markdown/plain-text notes pinned to the canvas,
  independent of any session.
- Keep what already works: multiple named sessions per `claude`/`codex`,
  multi-account Claude isolation, resume-on-restart, and the existing
  outgoing-CLI-summarizes / incoming-CLI-opens-with-summary handoff.

## Non-goals

- No device portals (iOS/Android simulator embeds) — meaningless on Linux.
- No "Floors" workspace snapshots (APFS copy-on-write) — no Linux equivalent
  filesystem feature is assumed; may resurface later as a git-worktree-based
  feature, but not in this spec.
- No "Ombro" local AI companion — out of scope; the existing CLI-driven
  summarize-on-handoff already covers the one piece of it duet needs.
- No hand-drawn diagram/shape tools.
- No daemon, no multi-user, no remote access — single local user, single
  machine, same as before.

## Architecture

One Rust binary, GTK4 + libadwaita UI instead of ratatui. A single
`AdwApplicationWindow` hosts one custom canvas widget. Each session or note
is a node positioned on the canvas; the canvas itself only handles pan/zoom
and drawing link lines between nodes — it does not know about agents, PTYs,
or persistence.

Each session keeps managing its own PTY and child process exactly as today
(`portable-pty`), because:
- it gives raw access to the output byte stream, which agent-to-agent
  linking needs to tap and forward into another session's input, and
- the existing `missing_conversation` detection (scanning output for "no
  conversation found" etc., used to warn about stale resumes) depends on
  seeing raw bytes.

The only thing that changes under a session is the render sink: instead of
feeding bytes into a `vt100::Parser` for `ratatui`/`tui-term` to draw, bytes
are fed into a `vte4::Terminal` widget, which does its own rendering,
scrollback, selection, clipboard, and URL detection. VTE's own input (key
presses, paste) is wired to the same PTY writer the session already owns via
its `commit` signal, instead of VTE spawning and owning the child itself —
this preserves duet's own process lifecycle management (clean kill on tab
close, exit-status tracking for the `×` badge equivalent).

## Components

- **`session.rs`** (renamed from `tab.rs`) — drops `vt100::Parser` and the
  `tui_term` dependency. Keeps `portable-pty` spawn/resize/kill/exit-status
  exactly as today. Exposes the raw output `Receiver<Vec<u8>>` publicly
  (previously consumed internally) so the GTK layer can both feed a
  session's own `vte4::Terminal` and, when linked, forward the same bytes
  into another session's `write_input`.
- **`canvas.rs`** (new) — a `gtk::Widget` subclass. Owns a zoom factor and
  pan offset. A `GtkGestureDrag` on empty canvas background pans; a
  `GtkEventControllerScroll` with Ctrl (or a `GtkGestureZoom` for touch/
  trackpad pinch) zooms, centered on the pointer. Child nodes are placed
  with `Fixed`-style manual allocation, positioned in canvas space and
  mapped to widget space through the current pan/zoom transform. The
  canvas's own `snapshot()` draws link lines (Cairo bezier/straight lines)
  between linked node anchor points, computed after children are allocated.
- **`node.rs`** (new) — two node kinds sharing one draggable chrome (an
  `adw::Bin` with a small title bar: name, agent badge, link/close buttons,
  grabbable to reposition on the canvas):
  - *Session node*: wraps a `vte4::Terminal` bound to one `session.rs`
    session.
  - *Sticky note node*: wraps a `GtkTextView` in a tinted frame, Markdown/
    plain text, no PTY.
- **`link.rs`** (new) — a link is an ordered pair `(source_session,
  target_session)`. While a link exists, bytes read from the source
  session's output channel are written into the target session's PTY input
  (in addition to the source's own terminal rendering). Created via a
  "link" button on a session node that then click-targets another node;
  removed via a close control on the link line itself (click a link line to
  select it, then a key/button to delete).
- **`agent.rs`, `account.rs`, `handoff.rs`** — unchanged. These never
  touched ratatui and need no changes for the GUI port.
- **`store.rs`** — extended, see Data model below.
- **`main.rs`, `app.rs`** — rewritten as the GTK4/libadwaita application
  entry point and top-level state (replacing the ratatui event loop with
  GTK's main loop; session polling moves from a manual tick to a
  `glib::timeout_add_local` that drains each session's output channel and
  feeds any subscribed `vte4::Terminal`/link targets).
- **`ui.rs`, `tab.rs` (old), `action.rs`** — deleted. Ratatui-specific
  drawing and the input-dispatch `Action` enum have no equivalent need once
  GTK's native widget event handling and signals take over.

## Data model (extends `store.rs`)

```rust
struct SessionRecord {
    // existing fields unchanged: name, cwd, agent, claude_session_id,
    // claude_account, codex_used
    position: (f64, f64),   // canvas-space top-left
    size: (f64, f64),       // canvas-space width/height
}

struct StickyNoteRecord {
    id: Uuid,
    text: String,
    position: (f64, f64),
    size: (f64, f64),
    color: String,           // one of a small fixed palette, not freeform
}

struct LinkRecord {
    source: Uuid,             // session id
    target: Uuid,             // session id
}

struct CanvasRecord {
    zoom: f64,
    pan: (f64, f64),
}

struct Store {
    sessions: Vec<SessionRecord>,   // renamed from `tabs`
    notes: Vec<StickyNoteRecord>,
    links: Vec<LinkRecord>,
    canvas: CanvasRecord,
}
```

`Store::load_with_warning` keeps its existing corrupt-file-backup behavior.
Loading an old `tabs.json` (no `position`/`size`/`notes`/`links`/`canvas`
fields) defaults `canvas` to zoom 1.0 / pan (0, 0) and lays out existing
sessions on a simple grid by index — a one-time migration, not an ongoing
compatibility shim.

## Data flow

1. On startup, `Store::load_with_warning` reads the JSON file; for each
   `SessionRecord`, `app.rs` spawns a `session.rs::Session` (resuming via
   each agent's existing resume mechanism, same as today) and a matching
   session node placed at its saved position.
2. A `glib::timeout_add_local` (replacing the old manual redraw-on-tick)
   runs every ~16ms: for each session, `pull_output` drains its PTY
   channel; new bytes feed that session's own `vte4::Terminal` via `feed()`
   and, for each active outgoing link, are also written into the target
   session's PTY input.
3. User input into a session node's `vte4::Terminal` triggers VTE's
   `commit` signal with the typed/pasted bytes; the handler writes them to
   that session's PTY input exactly as keystrokes did in the TUI.
4. Dragging a node, panning, zooming, creating/removing a link, or editing
   a sticky note marks the store dirty; a debounced save (same atomic
   write-temp-then-rename as today) persists `store.rs`'s state shortly
   after the last change, not on every pixel of a drag.
5. Handoff (agent switch) is unchanged in logic: summarize via
   `handoff.rs`, kill the old session, spawn a new one with the summary as
   initial prompt, same node position/size retained.

## Error handling

- Same philosophy as today: a session's process exiting is reflected as a
  status badge on its node (not a crash of duet); PTY spawn failures surface
  as a dismissible in-app toast (`adw::Toast`) rather than a panel like the
  TUI's old error line.
- A link whose target session has exited is dropped silently (bytes have
  nowhere to go); the link line is removed from the canvas and the record
  pruned on next save.
- Store corruption handling (`.corrupt.json` backup) is unchanged.

## Testing

- `session.rs`, `agent.rs`, `account.rs`, `store.rs`, `handoff.rs` keep
  their existing unit tests; `session.rs`'s tests drop any assertions tied
  to the `vt100` screen buffer (replaced by output-channel-level
  assertions, since rendering is now VTE's responsibility, not ours to
  test).
- `canvas.rs` gets unit tests for the pure coordinate math (canvas-space
  <-> widget-space transform under a given pan/zoom) without needing a
  real display.
- `link.rs` gets a unit test that bytes written to a source session's
  output channel are observed on a linked target session's input, using
  two real `portable-pty` sessions running `cat`/`sh`, the same style as
  existing `session.rs` tests.
- No automated test drives actual GTK widget rendering (standard practice —
  GTK UI itself is verified manually via the `run` workflow, not asserted
  in CI).

## Migration notes

- `Cargo.toml`: remove `ratatui`, `tui-term`, `crossterm`; add `gtk4 =
  "0.11"`, `libadwaita = "0.9"`, `vte4 = "0.10"`, `glib` (pulled in
  transitively by gtk4-rs).
- Binary name, install instructions, and `CLAUDE_CONFIG_DIR`-based account
  isolation are unaffected by this change.
- The `F1`/`Ctrl+T`/etc. keybindings in the current README reflect ratatui
  dispatch and will be redone as GTK accelerators/menu actions; exact
  bindings are an implementation-plan detail, not fixed here.
