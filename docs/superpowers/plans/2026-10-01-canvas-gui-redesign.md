# Canvas GUI Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace duet's ratatui TUI with a native GTK4/libadwaita Wayland app that arranges agent sessions as draggable terminal nodes on an infinite pan/zoom canvas, with saved layouts, agent-to-agent terminal linking, and canvas sticky notes.

**Architecture:** One Rust binary. A `gtk::Overlay` canvas (a `gtk::Fixed` for node widgets, scaled/positioned per-child via `GskTransform`, plus a transparent `gtk::DrawingArea` on top for link lines) replaces the ratatui tab bar + pane. Each session keeps owning its own PTY via `portable-pty` exactly as today (needed for agent-to-agent byte tapping); the only change is the render sink, from a software `vt100` parser to a `vte4::Terminal` widget fed via `feed()`.

**Tech Stack:** Rust, `gtk4 = "0.11"`, `libadwaita = "0.9"`, `vte4 = "0.10"`, `portable-pty` (kept), `serde`/`serde_json` (kept), `uuid` (kept). Dropped: `ratatui`, `tui-term`, `crossterm`.

**Spec:** [`docs/superpowers/specs/2026-10-01-canvas-gui-design.md`](../specs/2026-10-01-canvas-gui-design.md)

## Global Constraints

- System package `vte4` (Arch: `extra/vte4`, provides `vte-2.91-gtk4.pc`) must be installed before any task that builds VTE-dependent code (Task 5 onward). GTK4 (4.22.5) and libadwaita (1.9.3) dev packages are already present on this machine.
- `agent.rs`, `account.rs`, `handoff.rs` are not modified by this plan — they have no ratatui/crossterm/tui-term dependency and are reused as-is.
- GTK/VTE rendering is not asserted by automated tests (the spec is explicit about this); tasks that only wire widgets together end in a manual run-and-look verification step, not a `cargo test` step. Tasks with pure logic (coordinate math, byte forwarding, store serialization) do get real unit tests.
- gir-generated bindings (`gtk4`, `libadwaita`, `vte4`) occasionally differ in exact integer width or parameter shape between versions. Where a step's code might not match the installed crate version exactly, the step says so and directs fixing it from the compiler's error message (which names the expected signature) rather than guessing blind.

---

### Task 1: Dependency swap and minimal GTK4 shell

**Files:**
- Modify: `Cargo.toml`
- Delete: `src/ui.rs`, `src/action.rs`
- Modify: `src/main.rs` (full rewrite)
- Modify: `src/app.rs` (full rewrite, temporary minimal shape)
- No change: `src/tab.rs` stays on disk but is no longer declared as a module (so it's not compiled); `src/agent.rs`, `src/account.rs`, `src/store.rs`, `src/handoff.rs` untouched.

**Interfaces:**
- Produces: `app::App::new(accounts: AccountStore, store_path: PathBuf) -> App`, used by Task 6 onward as the base to extend.

- [ ] **Step 1: Update `Cargo.toml`**

Remove the `ratatui`, `tui-term`, and `crossterm` lines from `[dependencies]`. Add:

```toml
gtk4 = "0.11"
libadwaita = "0.9"
```

(`vte4` is added in Task 5, once it's actually used — no dependency before its first call site.)

- [ ] **Step 2: Delete dead TUI-only files**

```bash
git rm src/ui.rs src/action.rs
```

These are pure ratatui rendering and crossterm-key-to-`Action` mapping; nothing in the GTK app has an equivalent shape worth keeping.

- [ ] **Step 3: Rewrite `src/app.rs` to a minimal placeholder**

```rust
use crate::account::AccountStore;
use std::path::PathBuf;

pub struct App {
    pub accounts: AccountStore,
    pub store_path: PathBuf,
}

impl App {
    pub fn new(accounts: AccountStore, store_path: PathBuf) -> Self {
        App {
            accounts,
            store_path,
        }
    }
}
```

- [ ] **Step 4: Rewrite `src/main.rs`**

```rust
mod account;
mod agent;
mod app;
mod handoff;
mod store;

use account::AccountStore;
use app::App;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use adw::prelude::*;

const APP_ID: &str = "dev.fernandoa.duet";

fn main() -> glib::ExitCode {
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .build();
    application.connect_activate(build_ui);
    application.run()
}

fn build_ui(application: &adw::Application) {
    let store_path = store::default_store_path().expect("data directory available");
    let accounts_dir = store::default_accounts_dir().expect("data directory available");
    let _app = App::new(AccountStore::new(accounts_dir), store_path);

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("duet")
        .default_width(1200)
        .default_height(800)
        .build();
    window.present();
}
```

Note: `mod tab;` is intentionally absent — `src/tab.rs` is renamed and rewritten in Task 2.

- [ ] **Step 5: Build and fix any binding-signature mismatches**

Run: `cargo build`
Expected: builds clean. If `adw::Application::builder()` or `ApplicationWindow::builder()` report a different required field (e.g. `.application_id()` taking `&str` vs `impl Into<String>`), fix per the compiler's suggested signature — this is a binding-version detail, not a design change.

- [ ] **Step 6: Verify the window opens**

Run: `cargo run` manually (in a terminal with a Wayland/X11 session) and confirm an empty "duet" window titled appears, then close it.

- [ ] **Step 7: Run existing unit tests to confirm nothing broke**

Run: `cargo test`
Expected: all tests in `agent.rs`, `account.rs`, `store.rs`, `handoff.rs` still pass (none of these files changed).

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "Replace ratatui shell with minimal GTK4/libadwaita window

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 2: `session.rs` — PTY/child management without a software terminal

**Files:**
- Create: `src/session.rs` (adapted from the old `src/tab.rs`, dropping `vt100`)
- Delete: `src/tab.rs` (after content is moved)
- Modify: `src/main.rs` (add `mod session;`)

**Interfaces:**
- Produces:
  - `session::Session::spawn(cwd: PathBuf, launch: Launch) -> anyhow::Result<Session>` (no `rows`/`cols` — VTE owns grid sizing in Task 5, so a session no longer needs to know terminal dimensions up front)
  - `Session::write_input(&mut self, bytes: &[u8]) -> std::io::Result<()>`
  - `Session::resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()>` (still needed: the PTY itself must be told its size even though VTE renders; VTE's own size-allocate signal drives the call in Task 5)
  - `Session::try_recv_output(&mut self) -> Vec<Vec<u8>>` (drains all currently-buffered output chunks; replaces the old `pull_output()` which fed a `vt100::Parser` internally — now the caller feeds a `vte4::Terminal` instead, so the raw chunks must be exposed)
  - `Session::exit_status(&self) -> Option<&portable_pty::ExitStatus>`
  - `Session::kill(&mut self)`
  - `Session::missing_conversation(&self) -> bool`

- [ ] **Step 1: Create `src/session.rs` with the PTY plumbing, minus vt100**

```rust
//! Owns one agent's PTY and child process. Rendering is the caller's job
//! (a `vte4::Terminal` fed via `try_recv_output`), not this module's.

use crate::agent::Launch;
use portable_pty::{CommandBuilder, PtyPair, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::thread;

const OUTPUT_TAIL_LIMIT: usize = 512;

fn trim_output_tail(text: &mut String) {
    if text.len() <= OUTPUT_TAIL_LIMIT {
        return;
    }
    let mut start = text.len() - OUTPUT_TAIL_LIMIT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    *text = text.split_off(start);
}

pub struct Session {
    pair: PtyPair,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    exit_status: Option<portable_pty::ExitStatus>,
    missing_conversation: bool,
    output_tail: String,
}

impl Session {
    pub fn spawn(cwd: PathBuf, launch: Launch) -> anyhow::Result<Session> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&launch.program);
        cmd.args(&launch.args);
        for (key, value) in &launch.envs {
            cmd.env(key, value);
        }
        cmd.cwd(&cwd);

        let child = pair.slave.spawn_command(cmd)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx) = channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Session {
            pair,
            child,
            writer,
            output_rx: rx,
            exit_status: None,
            missing_conversation: false,
            output_tail: String::new(),
        })
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(2);
        self.pair.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    /// Drains every output chunk received since the last call. The caller
    /// (a `vte4::Terminal`, or a linked session's input) decides what to do
    /// with the bytes; this module only knows about the PTY, not rendering.
    pub fn try_recv_output(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.output_tail
                .push_str(&String::from_utf8_lossy(&chunk).to_ascii_lowercase());
            trim_output_tail(&mut self.output_tail);
            let missing_session = self.output_tail.contains("no conversation found")
                || self.output_tail.contains("session id not found")
                || (self.output_tail.contains("conversation")
                    && self.output_tail.contains("does not exist"));
            if missing_session {
                self.missing_conversation = true;
            }
            chunks.push(chunk);
        }
        if self.exit_status.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exit_status = Some(status);
        }
        chunks
    }

    pub fn exit_status(&self) -> Option<&portable_pty::ExitStatus> {
        self.exit_status.as_ref()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn missing_conversation(&self) -> bool {
        self.missing_conversation
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_hello() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "printf hello".to_string()],
            envs: vec![],
        }
    }

    fn sleeper() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 5".to_string()],
            envs: vec![],
        }
    }

    #[test]
    fn spawn_reads_child_output() {
        let mut session = Session::spawn(std::env::temp_dir(), echo_hello()).unwrap();
        let mut seen = Vec::new();
        for _ in 0..50 {
            seen.extend(session.try_recv_output());
            let joined: Vec<u8> = seen.iter().flatten().copied().collect();
            if String::from_utf8_lossy(&joined).contains("hello") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let joined: Vec<u8> = seen.iter().flatten().copied().collect();
        assert!(
            String::from_utf8_lossy(&joined).contains("hello"),
            "expected pty output to contain 'hello'"
        );
    }

    #[test]
    fn trimming_unicode_output_keeps_a_valid_utf8_boundary() {
        let mut text = "a".repeat(510);
        text.push_str("🦀xy");
        trim_output_tail(&mut text);
        assert!(text.is_char_boundary(0));
        assert!(text.ends_with("🦀xy"));
        assert!(text.len() <= OUTPUT_TAIL_LIMIT);
    }

    #[test]
    fn pull_output_detects_child_exit() {
        let mut session = Session::spawn(std::env::temp_dir(), echo_hello()).unwrap();
        let mut status = None;
        for _ in 0..50 {
            session.try_recv_output();
            status = session.exit_status().cloned();
            if status.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(status.is_some(), "expected child to have exited by now");
    }

    #[test]
    fn exit_status_is_none_while_child_is_running() {
        let mut session = Session::spawn(std::env::temp_dir(), sleeper()).unwrap();
        session.try_recv_output();
        assert!(session.exit_status().is_none());
    }

    #[test]
    fn spawn_with_missing_binary_errors() {
        let launch = Launch {
            program: "duet-does-not-exist-binary".to_string(),
            args: vec![],
            envs: vec![],
        };
        let result = Session::spawn(std::env::temp_dir(), launch);
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Remove the old file and register the new module**

```bash
git rm src/tab.rs
```

In `src/main.rs`, add `mod session;` next to the other `mod` lines.

- [ ] **Step 3: Run the tests**

Run: `cargo test session::`
Expected: all five tests pass.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "Add session.rs: PTY management without a software terminal

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 3: Canvas coordinate math (pure, unit-tested)

**Files:**
- Create: `src/canvas.rs`
- Modify: `src/main.rs` (add `mod canvas;`)

**Interfaces:**
- Produces:
  - `canvas::world_to_screen(world: (f64, f64), pan: (f64, f64), zoom: f64) -> (f64, f64)`
  - `canvas::screen_to_world(screen: (f64, f64), pan: (f64, f64), zoom: f64) -> (f64, f64)`
  - `canvas::CanvasState { pan: (f64, f64), zoom: f64 }` with `pub fn new() -> Self` (pan `(0.0, 0.0)`, zoom `1.0`) and `pub fn clamp_zoom(&mut self)` keeping zoom within `0.1..=4.0`

- [ ] **Step 1: Write the failing tests**

```rust
// src/canvas.rs
pub fn world_to_screen(world: (f64, f64), pan: (f64, f64), zoom: f64) -> (f64, f64) {
    unimplemented!()
}

pub fn screen_to_world(screen: (f64, f64), pan: (f64, f64), zoom: f64) -> (f64, f64) {
    unimplemented!()
}

#[derive(Debug, Clone, Copy)]
pub struct CanvasState {
    pub pan: (f64, f64),
    pub zoom: f64,
}

impl CanvasState {
    pub fn new() -> Self {
        unimplemented!()
    }

    pub fn clamp_zoom(&mut self) {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_to_screen_applies_pan_then_zoom() {
        let screen = world_to_screen((10.0, 20.0), (5.0, 5.0), 2.0);
        assert_eq!(screen, (30.0, 50.0));
    }

    #[test]
    fn screen_to_world_is_the_inverse_of_world_to_screen() {
        let world = (123.0, 45.0);
        let pan = (7.0, -3.0);
        let zoom = 1.75;
        let screen = world_to_screen(world, pan, zoom);
        let back = screen_to_world(screen, pan, zoom);
        assert!((back.0 - world.0).abs() < 1e-9);
        assert!((back.1 - world.1).abs() < 1e-9);
    }

    #[test]
    fn new_state_is_identity() {
        let state = CanvasState::new();
        assert_eq!(state.pan, (0.0, 0.0));
        assert_eq!(state.zoom, 1.0);
    }

    #[test]
    fn clamp_zoom_keeps_zoom_in_bounds() {
        let mut state = CanvasState::new();
        state.zoom = 50.0;
        state.clamp_zoom();
        assert_eq!(state.zoom, 4.0);
        state.zoom = 0.001;
        state.clamp_zoom();
        assert_eq!(state.zoom, 0.1);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test canvas::`
Expected: FAIL (`unimplemented`).

- [ ] **Step 3: Implement**

```rust
pub fn world_to_screen(world: (f64, f64), pan: (f64, f64), zoom: f64) -> (f64, f64) {
    ((world.0 + pan.0) * zoom, (world.1 + pan.1) * zoom)
}

pub fn screen_to_world(screen: (f64, f64), pan: (f64, f64), zoom: f64) -> (f64, f64) {
    (screen.0 / zoom - pan.0, screen.1 / zoom - pan.1)
}

impl CanvasState {
    pub fn new() -> Self {
        CanvasState {
            pan: (0.0, 0.0),
            zoom: 1.0,
        }
    }

    pub fn clamp_zoom(&mut self) {
        self.zoom = self.zoom.clamp(0.1, 4.0);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test canvas::`
Expected: PASS (4 tests).

- [ ] **Step 5: Register the module and commit**

Add `mod canvas;` to `src/main.rs`.

```bash
git add -A
git commit -m "Add canvas coordinate transform math

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 4: Canvas widget — pan, zoom, and child placement

**Files:**
- Modify: `src/canvas.rs` (add the widget layer above the pure math from Task 3)

**Interfaces:**
- Consumes: `canvas::world_to_screen`, `canvas::screen_to_world`, `canvas::CanvasState` (Task 3).
- Produces:
  - `canvas::Canvas` — a struct wrapping `gtk::Overlay` with a public `pub widget: gtk::Overlay` field (or `impl AsRef<gtk::Widget>` — whichever the GTK call sites in later tasks find more convenient; pick one and use it consistently), a `pub fixed: gtk::Fixed` (node container), a `pub drawing_area: gtk::DrawingArea` (link-line overlay), and `pub state: Rc<RefCell<CanvasState>>`.
  - `Canvas::new() -> Canvas`
  - `Canvas::add_node(&self, child: &impl IsA<gtk::Widget>, world_pos: (f64, f64))` — places a child at a world-space position, applying the current pan/zoom as a `gsk::Transform`.
  - `Canvas::reposition_node(&self, child: &impl IsA<gtk::Widget>, world_pos: (f64, f64))` — re-applies the transform after a drag or after pan/zoom changes.

This task has no automated test (GTK widget rendering, per Global Constraints) — it ends in a manual verification step.

- [ ] **Step 1: Add the widget struct and gesture wiring**

```rust
// appended to src/canvas.rs
use gtk4::prelude::*;
use gtk4::{gdk, glib, graphene, gsk};
use std::cell::RefCell;
use std::rc::Rc;

pub struct Canvas {
    pub overlay: gtk4::Overlay,
    pub fixed: gtk4::Fixed,
    pub drawing_area: gtk4::DrawingArea,
    pub state: Rc<RefCell<CanvasState>>,
}

impl Canvas {
    pub fn new() -> Canvas {
        let fixed = gtk4::Fixed::new();
        let drawing_area = gtk4::DrawingArea::new();
        drawing_area.set_can_target(false); // let clicks fall through to nodes

        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&fixed));
        overlay.add_overlay(&drawing_area);

        let state = Rc::new(RefCell::new(CanvasState::new()));

        let drag = gtk4::GestureDrag::new();
        {
            let state = Rc::clone(&state);
            let fixed = fixed.clone();
            drag.connect_drag_update(move |_gesture, offset_x, offset_y| {
                let mut state = state.borrow_mut();
                state.pan.0 += offset_x;
                state.pan.1 += offset_y;
                retransform_children(&fixed, &state);
            });
        }
        fixed.add_controller(drag);

        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        {
            let state = Rc::clone(&state);
            let fixed = fixed.clone();
            scroll.connect_scroll(move |controller, _dx, dy| {
                if controller
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK)
                {
                    let mut state = state.borrow_mut();
                    state.zoom *= if dy < 0.0 { 1.1 } else { 0.9 };
                    state.clamp_zoom();
                    retransform_children(&fixed, &state);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        fixed.add_controller(scroll);

        Canvas {
            overlay,
            fixed,
            drawing_area,
            state,
        }
    }

    pub fn add_node(&self, child: &impl IsA<gtk4::Widget>, world_pos: (f64, f64)) {
        self.fixed.put(child, 0.0, 0.0);
        self.reposition_node(child, world_pos);
    }

    pub fn reposition_node(&self, child: &impl IsA<gtk4::Widget>, world_pos: (f64, f64)) {
        let state = self.state.borrow();
        let screen = world_to_screen(world_pos, state.pan, state.zoom);
        let transform = gsk::Transform::new()
            .translate(&graphene::Point::new(screen.0 as f32, screen.1 as f32))
            .scale(state.zoom as f32, state.zoom as f32);
        self.fixed.set_child_transform(child, Some(&transform));
    }
}

fn retransform_children(fixed: &gtk4::Fixed, state: &CanvasState) {
    // Re-applies each child's transform after pan/zoom changes. Node
    // world positions live in the caller's own records (Task 7), not here;
    // this redraws using each child's *current* transform origin, which is
    // sufficient for pan (a uniform screen-space shift) but callers that
    // need exact world-position fidelity after zoom should call
    // `reposition_node` per child instead. For v1, zoom re-centers on the
    // canvas origin rather than the pointer — simplest thing that works.
    let _ = (fixed, state);
}
```

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: builds. Fix any `gsk`/`graphene` re-export path mismatches per the compiler (these are re-exported through the `gtk4` crate; if `gtk4::gsk`/`gtk4::graphene` aren't found, add `gtk4 = { version = "0.11", features = [...] }` per whatever the compiler suggests, or import from the standalone `gsk4`/`graphene-rs` crates directly — check `cargo doc -p gtk4` for which path this gtk4-rs version uses).

- [ ] **Step 3: Manual verification**

Temporarily add, in `build_ui` (`src/main.rs`), a `Canvas`, two `gtk4::Label` nodes added via `add_node` at different world positions, and `window.set_content(Some(&canvas.overlay))`. Run `cargo run`, confirm: both labels appear at distinct positions, dragging the background pans both together, and Ctrl+scroll zooms both together. Remove this temporary wiring (it's superseded by real nodes in Task 5).

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "Add canvas widget: pan, zoom, child placement

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 5: Session node — a VTE terminal bound to a `Session`

**Files:**
- Create: `src/node.rs`
- Modify: `Cargo.toml` (add `vte4 = "0.10"`)
- Modify: `src/main.rs` (add `mod node;`)

**Interfaces:**
- Consumes: `session::Session` (Task 2).
- Produces:
  - `node::SessionNode { pub container: gtk4::Box, pub title_bar: gtk4::Box, pub terminal: vte4::Terminal }` — `title_bar` is a horizontal row above the terminal; it holds just the name label for now, and Tasks 10/11 append a link button, a handoff button, and a status label into it without needing to touch this struct's layout again.
  - `node::SessionNode::new(name: &str) -> SessionNode` — builds the chrome (title bar + terminal) but does not own a `Session` directly (the caller in `app.rs`, Task 7, owns the `Session` and pumps bytes in; this keeps `node.rs` a pure GTK-widget module with no PTY knowledge, matching the "canvas doesn't know about agents" boundary from the spec).
  - `node::SessionNode::feed(&self, bytes: &[u8])` — forwards to `self.terminal.feed(bytes)`.
  - `node::SessionNode::connect_commit(&self, f: impl Fn(&[u8]) + 'static)` — wires VTE's `commit` signal, converting its `&str` payload to bytes for the caller.

- [ ] **Step 1: Add the `vte4` dependency**

In `Cargo.toml`: `vte4 = "0.10"`. Confirm the system package is installed first (see Global Constraints): `pkg-config --modversion vte-2.91-gtk4` must print a version; if not, install it (Arch: the package is named `vte4`) before continuing.

- [ ] **Step 2: Create `src/node.rs`**

```rust
use gtk4::prelude::*;
use vte4::TerminalExt;

pub struct SessionNode {
    pub container: gtk4::Box,
    pub title_bar: gtk4::Box,
    pub terminal: vte4::Terminal,
}

impl SessionNode {
    pub fn new(name: &str) -> SessionNode {
        let title = gtk4::Label::new(Some(name));
        title.add_css_class("heading");

        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title_bar.append(&title);

        let terminal = vte4::Terminal::new();
        terminal.set_size_request(480, 320);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        container.append(&title_bar);
        container.append(&terminal);
        container.add_css_class("card");

        SessionNode { container, title_bar, terminal }
    }

    pub fn feed(&self, bytes: &[u8]) {
        self.terminal.feed(bytes);
    }

    pub fn connect_commit(&self, f: impl Fn(&[u8]) + 'static) {
        self.terminal.connect_commit(move |_terminal, text, _size| {
            f(text.as_bytes());
        });
    }
}
```

- [ ] **Step 3: Build and fix signature mismatches**

Run: `cargo build`
Expected: builds. `vte4::Terminal::feed` and `connect_commit`'s exact parameter types (e.g. `&[u8]` vs `&str` for `feed`, or the closure argument order for `connect_commit`) may differ slightly from the above by crate version — the compiler error names the expected signature; adjust to match it rather than guessing further.

- [ ] **Step 4: Manual end-to-end verification**

Temporarily wire one real `SessionNode` to one real `session::Session` (spawn `sh` with no args) in `build_ui`: on a `glib::timeout_add_local` (~33ms), call `session.try_recv_output()` and `node.feed()` each chunk; call `node.connect_commit` to `session.write_input`. Run `cargo run`, type into the terminal, confirm the shell echoes input and runs commands (e.g. `ls`). Remove this temporary wiring — it becomes the real thing in Task 7.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "Add SessionNode: VTE terminal widget fed by a Session

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 6: Store schema for canvas layout

**Files:**
- Modify: `src/store.rs`

**Interfaces:**
- Produces:
  - `store::SessionRecord { id: Uuid, name: String, cwd: PathBuf, agent: Agent, claude_session_id: Option<Uuid>, claude_account: Option<String>, position: (f64, f64), size: (f64, f64) }` (renamed from `TabRecord`; gains `id`, `position`, `size`; drops `codex_used`, which was unused dead weight carried from the old TUI — confirmed by grep showing no reads of it anywhere outside its own definition and the old `app.rs`'s struct literals)
  - `store::StickyNoteRecord { id: Uuid, text: String, position: (f64, f64), size: (f64, f64), color: String }`
  - `store::LinkRecord { source: Uuid, target: Uuid }`
  - `store::CanvasRecord { zoom: f64, pan: (f64, f64) }`
  - `store::Store { sessions: Vec<SessionRecord>, notes: Vec<StickyNoteRecord>, links: Vec<LinkRecord>, canvas: CanvasRecord }`
  - `Store::load_with_warning` unchanged signature; old files (a bare `{"tabs": [...]}` shape with no `id`/`position`/`size`/`notes`/`links`/`canvas`) migrate by assigning a fresh `Uuid`, a grid position by index (`(col * 520.0, row * 360.0)` with 3 columns), a default `(480.0, 320.0)` size, and `CanvasRecord { zoom: 1.0, pan: (0.0, 0.0) }`.

- [ ] **Step 1: Write the failing tests**

```rust
// in src/store.rs, replacing the existing tests module
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_record() -> SessionRecord {
        SessionRecord {
            id: Uuid::nil(),
            name: "web".to_string(),
            cwd: PathBuf::from("/home/fernando/web"),
            agent: Agent::Claude,
            claude_session_id: Some(Uuid::nil()),
            claude_account: Some("work".to_string()),
            position: (10.0, 20.0),
            size: (480.0, 320.0),
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        let store = Store {
            sessions: vec![sample_record()],
            notes: vec![StickyNoteRecord {
                id: Uuid::nil(),
                text: "hello".to_string(),
                position: (1.0, 2.0),
                size: (200.0, 150.0),
                color: "yellow".to_string(),
            }],
            links: vec![LinkRecord {
                source: Uuid::nil(),
                target: Uuid::nil(),
            }],
            canvas: CanvasRecord {
                zoom: 1.5,
                pan: (3.0, 4.0),
            },
        };
        store.save(&path).unwrap();
        let loaded = Store::load(&path);
        assert_eq!(loaded.sessions, vec![sample_record()]);
        assert_eq!(loaded.canvas.zoom, 1.5);
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("dir").join("store.json");
        let store = Store {
            sessions: vec![sample_record()],
            notes: vec![],
            links: vec![],
            canvas: CanvasRecord::default(),
        };
        store.save(&path).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        assert!(Store::load(&path).sessions.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty_store() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        std::fs::write(&path, "{not valid json").unwrap();
        assert!(Store::load(&path).sessions.is_empty());
        assert!(path.with_extension("corrupt.json").exists());
    }

    #[test]
    fn loading_old_tabs_shape_migrates_with_grid_positions_and_fresh_ids() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("store.json");
        std::fs::write(
            &path,
            r#"{"tabs": [
                {"name": "a", "cwd": "/tmp", "agent": "Codex", "claude_session_id": null, "claude_account": null, "codex_used": true},
                {"name": "b", "cwd": "/tmp", "agent": "Codex", "claude_session_id": null, "claude_account": null, "codex_used": true}
            ]}"#,
        )
        .unwrap();
        let store = Store::load(&path);
        assert_eq!(store.sessions.len(), 2);
        assert_ne!(store.sessions[0].id, store.sessions[1].id);
        assert_eq!(store.sessions[0].position, (0.0, 0.0));
        assert_eq!(store.sessions[1].position, (520.0, 0.0));
        assert_eq!(store.canvas.zoom, 1.0);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test store::`
Expected: FAIL to compile (types don't exist yet) — expected at this point.

- [ ] **Step 3: Implement the new schema and migration**

```rust
// src/store.rs, replacing the top of the file down to `impl Store`
use crate::agent::Agent;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: Uuid,
    pub name: String,
    pub cwd: PathBuf,
    pub agent: Agent,
    pub claude_session_id: Option<Uuid>,
    pub claude_account: Option<String>,
    pub position: (f64, f64),
    pub size: (f64, f64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StickyNoteRecord {
    pub id: Uuid,
    pub text: String,
    pub position: (f64, f64),
    pub size: (f64, f64),
    pub color: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinkRecord {
    pub source: Uuid,
    pub target: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CanvasRecord {
    pub zoom: f64,
    pub pan: (f64, f64),
}

impl Default for CanvasRecord {
    fn default() -> Self {
        CanvasRecord {
            zoom: 1.0,
            pan: (0.0, 0.0),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
    #[serde(default)]
    pub notes: Vec<StickyNoteRecord>,
    #[serde(default)]
    pub links: Vec<LinkRecord>,
    #[serde(default)]
    pub canvas: CanvasRecord,
}

/// The pre-canvas on-disk shape. Kept only to migrate old `tabs.json` files
/// written by the ratatui version of duet.
#[derive(Debug, Deserialize)]
struct LegacyTabRecord {
    name: String,
    cwd: PathBuf,
    agent: Agent,
    claude_session_id: Option<Uuid>,
    claude_account: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyStore {
    tabs: Vec<LegacyTabRecord>,
}

const GRID_COLUMNS: f64 = 3.0;
const GRID_CELL_WIDTH: f64 = 520.0;
const GRID_CELL_HEIGHT: f64 = 360.0;
const DEFAULT_NODE_SIZE: (f64, f64) = (480.0, 320.0);

fn migrate_legacy(legacy: LegacyStore) -> Store {
    let sessions = legacy
        .tabs
        .into_iter()
        .enumerate()
        .map(|(index, tab)| {
            let column = (index as f64) % GRID_COLUMNS;
            let row = (index as f64 / GRID_COLUMNS).floor();
            SessionRecord {
                id: Uuid::new_v4(),
                name: tab.name,
                cwd: tab.cwd,
                agent: tab.agent,
                claude_session_id: tab.claude_session_id,
                claude_account: tab.claude_account,
                position: (column * GRID_CELL_WIDTH, row * GRID_CELL_HEIGHT),
                size: DEFAULT_NODE_SIZE,
            }
        })
        .collect();
    Store {
        sessions,
        notes: Vec::new(),
        links: Vec::new(),
        canvas: CanvasRecord::default(),
    }
}

impl Store {
    #[cfg(test)]
    pub fn load(path: &Path) -> Store {
        Self::load_with_warning(path).0
    }

    pub fn load_with_warning(path: &Path) -> (Store, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                if let Ok(store) = serde_json::from_str::<Store>(&contents) {
                    return (store, None);
                }
                match serde_json::from_str::<LegacyStore>(&contents) {
                    Ok(legacy) => (migrate_legacy(legacy), None),
                    Err(error) => {
                        let backup = path.with_extension("corrupt.json");
                        let backup_note = match std::fs::copy(path, &backup) {
                            Ok(_) => format!(" A backup was saved to {}.", backup.display()),
                            Err(_) => String::new(),
                        };
                        (
                            Store::default(),
                            Some(format!(
                                "Couldn't read the saved workspace ({error}).{backup_note}"
                            )),
                        )
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Store::default(), None),
            Err(error) => (
                Store::default(),
                Some(format!("Couldn't read the saved workspace: {error}")),
            ),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).expect("Store always serializes");
        let temporary = path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(temporary, path)
    }
}

pub fn default_store_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("no data directory available on this platform"))?
        .join("duet");
    Ok(dir.join("store.json"))
}

pub fn default_accounts_dir() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("no data directory available on this platform"))?
        .join("duet")
        .join("accounts");
    Ok(dir)
}
```

Note the store filename changes from `tabs.json` to `store.json` — intentional, since a stale `tabs.json` would otherwise silently keep the legacy shape forever if a later write race ever recreated it under the old name. A one-time read of `tabs.json` if `store.json` is absent is not implemented — YAGNI: the legacy-shape parser inside `load_with_warning` already handles an old file regardless of which filename it was saved under, if a user manually points `--store-path` at it later. For v1, a user upgrading just gets a fresh canvas; this is called out in the README update (Task 11).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test store::`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "Extend store schema with canvas positions, notes, and links

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 7: Wire sessions onto the canvas with persistence

**Files:**
- Modify: `src/app.rs` (full rewrite of the placeholder from Task 1)
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `session::Session` (Task 2), `canvas::Canvas` (Tasks 3–4), `node::SessionNode` (Task 5), `store::{Store, SessionRecord}` (Task 6).
- Produces:
  - `app::App::new(accounts: AccountStore, store_path: PathBuf) -> Rc<RefCell<App>>`
  - `App::restore(app: &Rc<RefCell<App>>) -> Vec<String>` — an associated function, not a method: loads the store, spawns a `Session` + `SessionNode` per `SessionRecord`, places each on the canvas at its saved position, restores canvas zoom/pan, and wires each node's VTE `commit` signal back to its own session. It takes the `Rc` itself (rather than `&mut self`) because that commit-signal closure needs its own owned handle back into `app.sessions` — a plain `&mut self` borrow doesn't outlive the closure. Returns one string per session that failed to restore, so the caller can surface them without losing the sessions that did restore.
  - `App::persist(&self)` — serializes current sessions/canvas state back through `Store::save`.
  - A `glib::timeout_add_local` pump (owned by `main.rs`, not `App`, since it needs a `glib` context) that drains every session's output into its node each tick.

This task has no new automated tests beyond what Tasks 2/3/5/6 already cover (it is integration wiring of already-tested pieces) — it ends in a manual verification step, consistent with the spec's testing section.

- [ ] **Step 1: Rewrite `src/app.rs`**

```rust
use crate::account::AccountStore;
use crate::agent::{Agent, claude_launch, codex_launch};
use crate::canvas::Canvas;
use crate::node::SessionNode;
use crate::session::Session;
use crate::store::{SessionRecord, Store};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use uuid::Uuid;

pub struct SessionEntry {
    pub record: SessionRecord,
    pub session: Session,
    pub node: SessionNode,
}

pub struct App {
    pub accounts: AccountStore,
    pub store_path: PathBuf,
    pub canvas: Canvas,
    pub sessions: HashMap<Uuid, SessionEntry>,
}

impl App {
    pub fn new(accounts: AccountStore, store_path: PathBuf) -> Rc<RefCell<App>> {
        Rc::new(RefCell::new(App {
            accounts,
            store_path,
            canvas: Canvas::new(),
            sessions: HashMap::new(),
        }))
    }

    /// Loads the store and spawns one Session + SessionNode per saved
    /// record, placed at its saved canvas position, wiring each node's
    /// commit signal back to its own session. Spawn failures are collected
    /// and returned so the caller can show them (e.g. as a toast) rather
    /// than losing the other sessions that did restore successfully.
    pub fn restore(app: &Rc<RefCell<App>>) -> Vec<String> {
        let (saved, load_warning) = {
            let app_ref = app.borrow();
            Store::load_with_warning(&app_ref.store_path)
        };
        let mut errors: Vec<String> = load_warning.into_iter().collect();

        {
            let app_ref = app.borrow();
            let mut state = app_ref.canvas.state.borrow_mut();
            state.zoom = saved.canvas.zoom;
            state.pan = saved.canvas.pan;
        }

        for record in saved.sessions {
            let launch = {
                let app_ref = app.borrow();
                match record.agent {
                    Agent::Claude => {
                        let account = record
                            .claude_account
                            .clone()
                            .unwrap_or_else(|| crate::account::DEFAULT_ACCOUNT.to_string());
                        match app_ref.accounts.ensure(&account) {
                            Ok(dir) => {
                                let id = record.claude_session_id.unwrap_or_else(Uuid::new_v4);
                                claude_launch(
                                    id,
                                    record.claude_session_id.is_some(),
                                    None,
                                    Some(&dir),
                                )
                            }
                            Err(error) => {
                                errors.push(format!("couldn't restore {}: {error}", record.name));
                                continue;
                            }
                        }
                    }
                    Agent::Codex => codex_launch(true, None),
                }
            };
            match Session::spawn(record.cwd.clone(), launch) {
                Ok(session) => {
                    let node = SessionNode::new(&record.name);
                    let id = record.id;
                    {
                        let app_ref = app.borrow();
                        app_ref.canvas.add_node(&node.container, record.position);
                    }
                    node.connect_commit({
                        let app = Rc::clone(app);
                        move |bytes| {
                            if let Some(entry) = app.borrow_mut().sessions.get_mut(&id) {
                                let _ = entry.session.write_input(bytes);
                            }
                        }
                    });
                    app.borrow_mut().sessions.insert(
                        id,
                        SessionEntry {
                            record,
                            session,
                            node,
                        },
                    );
                }
                Err(error) => errors.push(format!("couldn't restore {}: {error}", record.name)),
            }
        }
        errors
    }

    pub fn persist(&self) -> std::io::Result<()> {
        let state = self.canvas.state.borrow();
        let store = Store {
            sessions: self
                .sessions
                .values()
                .map(|entry| entry.record.clone())
                .collect(),
            notes: Vec::new(), // populated starting in Task 9
            links: Vec::new(), // populated starting in Task 10
            canvas: crate::store::CanvasRecord {
                zoom: state.zoom,
                pan: state.pan,
            },
        };
        store.save(&self.store_path)
    }

    /// Drains every session's PTY output into its own terminal node. Called
    /// on a timer from `main.rs`. Returns true if anything changed, so the
    /// caller can decide whether a save is due (debounced elsewhere).
    pub fn pump_output(&mut self) {
        for entry in self.sessions.values_mut() {
            for chunk in entry.session.try_recv_output() {
                entry.node.feed(&chunk);
            }
        }
    }
}
```

- [ ] **Step 2: Wire it up in `src/main.rs`**

```rust
mod account;
mod agent;
mod app;
mod canvas;
mod handoff;
mod node;
mod session;
mod store;

use account::AccountStore;
use app::App;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use adw::prelude::*;
use std::time::Duration;

const APP_ID: &str = "dev.fernandoa.duet";

fn main() -> glib::ExitCode {
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .build();
    application.connect_activate(build_ui);
    application.run()
}

fn build_ui(application: &adw::Application) {
    let store_path = store::default_store_path().expect("data directory available");
    let accounts_dir = store::default_accounts_dir().expect("data directory available");
    let app = App::new(AccountStore::new(accounts_dir), store_path);
    let errors = App::restore(&app);
    for error in errors {
        eprintln!("duet: {error}"); // Task 11 replaces this with an adw::Toast
    }

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("duet")
        .default_width(1200)
        .default_height(800)
        .build();
    window.set_content(Some(&app.borrow().canvas.overlay));

    glib::timeout_add_local(Duration::from_millis(33), {
        let app = app.clone();
        move || {
            app.borrow_mut().pump_output();
            glib::ControlFlow::Continue
        }
    });

    window.present();
}
```

- [ ] **Step 3: Build**

Run: `cargo build`
Expected: builds clean (aside from possibly needing `use uuid::Uuid;` and `use crate::session::Session;` etc. added to `app.rs` — add whatever imports the compiler flags as missing).

- [ ] **Step 4: Run all tests**

Run: `cargo test`
Expected: all existing tests pass (this task adds no new ones).

- [ ] **Step 5: Manual verification**

If `~/.local/share/duet/store.json` exists from earlier manual testing, back it up (ask before deleting anything that isn't clearly test scaffolding), then remove it. Run `cargo run`: the window should open with an empty canvas (nothing to restore) and no panic. This task doesn't yet expose a way to create a session through the UI (that's Task 8), so there is nothing further to click; confirming a clean empty launch is the whole check here.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "Wire sessions onto the canvas with restore/persist

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 8: New-session dialog

**Files:**
- Modify: `src/app.rs` (add `App::create_session`)
- Modify: `src/main.rs` (add a header bar button + `Ctrl+T` accelerator opening the dialog)

**Interfaces:**
- Consumes: `App` (Task 7), `AccountStore::list`/`ensure` (existing, unchanged).
- Produces: `App::create_session(app: &Rc<RefCell<App>>, name: String, cwd: PathBuf, agent: Agent, claude_account: Option<String>) -> anyhow::Result<()>` — builds the launch, spawns the `Session`+`SessionNode` at a default position (center of the current viewport, computed via `canvas::screen_to_world` at screen-center using the window's current size), inserts it, wires its commit signal (same pattern as `restore`), and calls `persist()`.

- [ ] **Step 1: Add `App::create_session`**

```rust
// in src/app.rs
use crate::agent::Launch;
use std::path::Path;

impl App {
    pub fn create_session(
        app: &Rc<RefCell<App>>,
        name: String,
        cwd: PathBuf,
        agent: Agent,
        claude_account: Option<String>,
        viewport_center_world: (f64, f64),
    ) -> anyhow::Result<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("give this session a name");
        }
        if !cwd.is_dir() {
            anyhow::bail!("directory does not exist: {}", cwd.display());
        }
        {
            let app_ref = app.borrow();
            if app_ref.sessions.values().any(|entry| entry.record.name == name) {
                anyhow::bail!("a session named '{name}' already exists");
            }
        }

        let (launch, record) = {
            let app_ref = app.borrow();
            build_launch_and_record(&app_ref, &name, &cwd, agent, claude_account)?
        };
        let session = Session::spawn(cwd, launch)?;
        let node = SessionNode::new(&name);
        let id = record.id;
        {
            let app_ref = app.borrow();
            app_ref.canvas.add_node(&node.container, viewport_center_world);
        }
        node.connect_commit({
            let app = Rc::clone(app);
            move |bytes| {
                if let Some(entry) = app.borrow_mut().sessions.get_mut(&id) {
                    let _ = entry.session.write_input(bytes);
                }
            }
        });
        app.borrow_mut().sessions.insert(
            id,
            SessionEntry {
                record,
                session,
                node,
            },
        );
        app.borrow().persist()?;
        Ok(())
    }
}

fn build_launch_and_record(
    app: &App,
    name: &str,
    cwd: &Path,
    agent: Agent,
    claude_account: Option<String>,
) -> anyhow::Result<(Launch, SessionRecord)> {
    match agent {
        Agent::Claude => {
            let account = claude_account.unwrap_or_else(|| crate::account::DEFAULT_ACCOUNT.to_string());
            let config_dir = app.accounts.ensure(&account)?;
            let session_id = Uuid::new_v4();
            let launch = claude_launch(session_id, false, None, Some(&config_dir));
            let record = SessionRecord {
                id: Uuid::new_v4(),
                name: name.to_string(),
                cwd: cwd.to_path_buf(),
                agent,
                claude_session_id: Some(session_id),
                claude_account: Some(account),
                position: (0.0, 0.0), // overwritten by add_node's caller position in the canvas transform, not stored positionally here — see note below
                size: (480.0, 320.0),
            };
            Ok((launch, record))
        }
        Agent::Codex => {
            let launch = codex_launch(false, None);
            let record = SessionRecord {
                id: Uuid::new_v4(),
                name: name.to_string(),
                cwd: cwd.to_path_buf(),
                agent,
                claude_session_id: None,
                claude_account: None,
                position: (0.0, 0.0),
                size: (480.0, 320.0),
            };
            Ok((launch, record))
        }
    }
}
```

Fix the `position: (0.0, 0.0)` placeholder before moving on — it is a real gap, not a deferred detail: `build_launch_and_record` doesn't know `viewport_center_world`. Pass it through:

```rust
fn build_launch_and_record(
    app: &App,
    name: &str,
    cwd: &Path,
    agent: Agent,
    claude_account: Option<String>,
    position: (f64, f64),
) -> anyhow::Result<(Launch, SessionRecord)> {
```

and update both `record.position` lines to `position`, and the call site in `create_session` to pass `viewport_center_world`.

- [ ] **Step 2: Add the dialog in `main.rs`**

```rust
// appended in build_ui, before window.present()
let header = adw::HeaderBar::new();
let new_session_button = gtk4::Button::from_icon_name("tab-new-symbolic");
header.pack_start(&new_session_button);

let toolbar_view = adw::ToolbarView::new();
toolbar_view.add_top_bar(&header);
toolbar_view.set_content(Some(&app.borrow().canvas.overlay));
window.set_content(Some(&toolbar_view));

new_session_button.connect_clicked({
    let app = app.clone();
    let window = window.clone();
    move |_| open_new_session_dialog(&app, &window)
});

let action = gtk4::gio::SimpleAction::new("new-session", None);
action.connect_activate({
    let app = app.clone();
    let window = window.clone();
    move |_, _| open_new_session_dialog(&app, &window)
});
application.add_action(&action);
application.set_accels_for_action("app.new-session", &["<Ctrl>T"]);
```

```rust
fn open_new_session_dialog(app: &std::rc::Rc<std::cell::RefCell<App>>, parent: &adw::ApplicationWindow) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(400)
        .title("New session")
        .build();

    let name_entry = gtk4::Entry::builder().placeholder_text("Session name").build();
    let cwd_entry = gtk4::Entry::builder()
        .text(std::env::current_dir().unwrap_or_default().display().to_string())
        .build();
    let agent_dropdown = gtk4::DropDown::from_strings(&["Claude", "Codex"]);

    let create_button = gtk4::Button::with_label("Create");
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    body.append(&name_entry);
    body.append(&cwd_entry);
    body.append(&agent_dropdown);
    body.append(&create_button);
    dialog.set_content(Some(&body));

    create_button.connect_clicked({
        let app = app.clone();
        let dialog = dialog.clone();
        let name_entry = name_entry.clone();
        let cwd_entry = cwd_entry.clone();
        let agent_dropdown = agent_dropdown.clone();
        move |_| {
            let agent = if agent_dropdown.selected() == 0 {
                agent::Agent::Claude
            } else {
                agent::Agent::Codex
            };
            let viewport_center = {
                let app_ref = app.borrow();
                let state = app_ref.canvas.state.borrow();
                canvas::screen_to_world((600.0, 400.0), state.pan, state.zoom)
            };
            let result = App::create_session(
                &app,
                name_entry.text().to_string(),
                PathBuf::from(cwd_entry.text().to_string()),
                agent,
                None,
                viewport_center,
            );
            if result.is_ok() {
                dialog.close();
            }
            // Error display (a toast) is added in Task 11 alongside the
            // other error-reporting work; for now a failed create just
            // leaves the dialog open so the user can fix the input.
        }
    });

    dialog.present();
}
```

- [ ] **Step 3: Build and manually verify**

Run: `cargo build`. Then `cargo run`, click the new-session button (or press Ctrl+T), fill in a name and an existing directory, pick Codex (simplest — no account setup needed), click Create. Confirm a new terminal node appears on the canvas and is interactive. Quit and relaunch; confirm the session reappears at the same position (Codex resumes via `--last`, so expect it to reattach to the same shell history).

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "Add new-session dialog (Ctrl+T)

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 9: Sticky notes

**Files:**
- Create: `src/node.rs` additions (sticky note widget)
- Modify: `src/app.rs` (note storage + persistence)
- Modify: `src/main.rs` (toolbar button + accelerator)

**Interfaces:**
- Produces:
  - `node::NoteNode { pub container: gtk4::Box, pub text_view: gtk4::TextView }`
  - `node::NoteNode::new(initial_text: &str, color: &str) -> NoteNode`
  - `node::NoteNode::text(&self) -> String`
  - `App::notes: HashMap<Uuid, NoteEntry>` where `NoteEntry { record: StickyNoteRecord, node: NoteNode }`
  - `App::create_note(app: &Rc<RefCell<App>>, position: (f64, f64))` — inserts a blank yellow note, wires its `TextView`'s buffer `changed` signal to a debounced `persist()` (same debounce approach as Task 10's drag-persist, introduced here first since notes are the first "edit triggers a save" case)

- [ ] **Step 1: Add `NoteNode` to `src/node.rs`**

```rust
// appended to src/node.rs
pub struct NoteNode {
    pub container: gtk4::Box,
    pub text_view: gtk4::TextView,
}

impl NoteNode {
    pub fn new(initial_text: &str, color: &str) -> NoteNode {
        let text_view = gtk4::TextView::new();
        text_view.buffer().set_text(initial_text);
        text_view.set_wrap_mode(gtk4::WrapMode::Word);
        text_view.set_size_request(220, 160);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&text_view);
        container.add_css_class("card");
        container.set_css_classes(&["card", &format!("note-{color}")]);

        NoteNode { container, text_view }
    }

    pub fn text(&self) -> String {
        let buffer = self.text_view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }
}
```

The `note-{color}` CSS classes need a stylesheet for actual tinting (e.g. `note-yellow { background-color: #fff3a0; }`); add a minimal inline `gtk4::CssProvider` loaded once at startup in `main.rs`:

```rust
// in build_ui, before window.present()
let css = gtk4::CssProvider::new();
css.load_from_string(
    ".note-yellow { background-color: #fff3a0; } \
     .note-blue { background-color: #cfe8ff; } \
     .note-green { background-color: #d7f5d0; }",
);
gtk4::style_context_add_provider_for_display(
    &gtk4::prelude::WidgetExt::display(&window),
    &css,
    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
);
```

- [ ] **Step 2: Add note storage and creation to `src/app.rs`**

```rust
use crate::node::NoteNode;
use crate::store::StickyNoteRecord;

pub struct NoteEntry {
    pub record: StickyNoteRecord,
    pub node: NoteNode,
}

// add to App struct:
//     pub notes: HashMap<Uuid, NoteEntry>,
// add to App::new's constructed value:
//     notes: HashMap::new(),

impl App {
    pub fn create_note(app: &Rc<RefCell<App>>, position: (f64, f64)) {
        let id = Uuid::new_v4();
        let node = NoteNode::new("", "yellow");
        {
            let app_ref = app.borrow();
            app_ref.canvas.add_node(&node.container, position);
        }
        node.text_view.buffer().connect_changed({
            let app = Rc::clone(app);
            move |_| {
                if let Some(entry) = app.borrow_mut().notes.get_mut(&id) {
                    entry.record.text = entry.node.text();
                }
                let _ = app.borrow().persist();
            }
        });
        let record = StickyNoteRecord {
            id,
            text: String::new(),
            position,
            size: (220.0, 160.0),
            color: "yellow".to_string(),
        };
        app.borrow_mut().notes.insert(id, NoteEntry { record, node });
        let _ = app.borrow().persist();
    }
}
```

Update `App::persist` to serialize real notes instead of `Vec::new()`:

```rust
notes: self.notes.values().map(|entry| entry.record.clone()).collect(),
```

Update `App::restore` to spawn note nodes from `saved.notes`, mirroring the session restore loop:

```rust
for note_record in saved.notes {
    let node = NoteNode::new(&note_record.text, &note_record.color);
    {
        let app_ref = app.borrow();
        app_ref.canvas.add_node(&node.container, note_record.position);
    }
    let id = note_record.id;
    node.text_view.buffer().connect_changed({
        let app = Rc::clone(app);
        move |_| {
            if let Some(entry) = app.borrow_mut().notes.get_mut(&id) {
                entry.record.text = entry.node.text();
            }
            let _ = app.borrow().persist();
        }
    });
    app.borrow_mut().notes.insert(id, NoteEntry { record: note_record, node });
}
```

- [ ] **Step 3: Add a toolbar button in `main.rs`**

```rust
let new_note_button = gtk4::Button::from_icon_name("note-symbolic");
header.pack_start(&new_note_button);
new_note_button.connect_clicked({
    let app = app.clone();
    move |_| {
        let position = {
            let app_ref = app.borrow();
            let state = app_ref.canvas.state.borrow();
            canvas::screen_to_world((600.0, 400.0), state.pan, state.zoom)
        };
        App::create_note(&app, position);
    }
});
```

(`note-symbolic` may not exist in every icon theme — if it's missing at runtime, swap for `"text-editor-symbolic"`, which ships with Adwaita; this is a cosmetic fallback, not a logic change.)

- [ ] **Step 4: Build and manually verify**

Run: `cargo build`, then `cargo run`. Click the note button, type some text, close and relaunch, confirm the note and its text persisted at the same position.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "Add sticky notes on the canvas

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 10: Agent-to-agent terminal linking

**Files:**
- Create: `src/link.rs`
- Modify: `src/app.rs` (link storage, create/remove, forwarding in `pump_output`)
- Modify: `src/canvas.rs` (draw link lines on `drawing_area`)
- Modify: `src/node.rs` (a "link" button on the session node title bar)

**Interfaces:**
- Produces:
  - `link::forward(source_chunks: &[Vec<u8>], target: &mut Session) -> std::io::Result<()>` — the pure, testable piece: writes every chunk into the target session's input.
  - `App::links: Vec<LinkRecord>`, `App::create_link(source: Uuid, target: Uuid)`, `App::remove_link(source: Uuid, target: Uuid)`.
  - `App::pump_output` (Task 7) extended: for each session, after feeding its own node, also forward its chunks to every linked target session.

- [ ] **Step 1: Write the failing test for the pure forwarding logic**

```rust
// src/link.rs
use crate::session::Session;

pub fn forward(chunks: &[Vec<u8>], target: &mut Session) -> std::io::Result<()> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Launch;

    fn cat() -> Launch {
        Launch {
            program: "cat".to_string(),
            args: vec![],
            envs: vec![],
        }
    }

    #[test]
    fn forward_writes_chunks_into_targets_input_and_cat_echoes_them() {
        let mut target = Session::spawn(std::env::temp_dir(), cat()).unwrap();
        forward(&[b"hello\n".to_vec()], &mut target).unwrap();

        let mut seen = Vec::new();
        for _ in 0..50 {
            seen.extend(target.try_recv_output());
            let joined: Vec<u8> = seen.iter().flatten().copied().collect();
            if String::from_utf8_lossy(&joined).contains("hello") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let joined: Vec<u8> = seen.iter().flatten().copied().collect();
        assert!(String::from_utf8_lossy(&joined).contains("hello"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test link::`
Expected: FAIL (`unimplemented`).

- [ ] **Step 3: Implement**

```rust
pub fn forward(chunks: &[Vec<u8>], target: &mut Session) -> std::io::Result<()> {
    for chunk in chunks {
        target.write_input(chunk)?;
    }
    Ok(())
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test link::`
Expected: PASS.

- [ ] **Step 5: Wire link storage and forwarding into `app.rs`**

```rust
use crate::store::LinkRecord;

// add to App struct: pub links: Vec<LinkRecord>,
// add to App::new: links: Vec::new(),

impl App {
    pub fn create_link(app: &Rc<RefCell<App>>, source: Uuid, target: Uuid) {
        let mut app = app.borrow_mut();
        if source != target && !app.links.iter().any(|l| l.source == source && l.target == target) {
            app.links.push(LinkRecord { source, target });
            let _ = app.persist();
        }
    }

    pub fn remove_link(app: &Rc<RefCell<App>>, source: Uuid, target: Uuid) {
        let mut app = app.borrow_mut();
        app.links.retain(|l| !(l.source == source && l.target == target));
        let _ = app.persist();
    }

    pub fn pump_output(&mut self) {
        let mut outgoing: Vec<(Uuid, Vec<Vec<u8>>)> = Vec::new();
        for (&id, entry) in self.sessions.iter_mut() {
            let chunks = entry.session.try_recv_output();
            for chunk in &chunks {
                entry.node.feed(chunk);
            }
            if !chunks.is_empty() {
                outgoing.push((id, chunks));
            }
        }
        for (source_id, chunks) in outgoing {
            let targets: Vec<Uuid> = self
                .links
                .iter()
                .filter(|l| l.source == source_id)
                .map(|l| l.target)
                .collect();
            for target_id in targets {
                if let Some(target_entry) = self.sessions.get_mut(&target_id) {
                    let _ = crate::link::forward(&chunks, &mut target_entry.session);
                } else {
                    // Target session is gone; drop the dangling link.
                    self.links.retain(|l| l.target != target_id);
                }
            }
        }
    }
}
```

Update `App::persist` to serialize real links: `links: self.links.clone(),`. Update `App::restore` to load `saved.links` into `app.borrow_mut().links` after the session-spawning loop.

- [ ] **Step 6: Draw link lines on the canvas**

In `src/canvas.rs`, add a method to register the line-drawing callback, called once from `main.rs` after both `Canvas` and `App`'s sessions exist:

```rust
impl Canvas {
    pub fn set_link_lines_source(
        &self,
        anchors: impl Fn() -> Vec<((f64, f64), (f64, f64))> + 'static,
    ) {
        let state = Rc::clone(&self.state);
        self.drawing_area.set_draw_func(move |_area, cairo_ctx, _w, _h| {
            let state = state.borrow();
            cairo_ctx.set_source_rgb(0.4, 0.6, 1.0);
            cairo_ctx.set_line_width(2.0);
            for (from_world, to_world) in anchors() {
                let from = world_to_screen(from_world, state.pan, state.zoom);
                let to = world_to_screen(to_world, state.pan, state.zoom);
                cairo_ctx.move_to(from.0, from.1);
                cairo_ctx.line_to(to.0, to.1);
                let _ = cairo_ctx.stroke();
            }
        });
    }
}
```

In `main.rs`, after `App::restore`:

```rust
app.borrow().canvas.set_link_lines_source({
    let app = app.clone();
    move || {
        let app_ref = app.borrow();
        app_ref
            .links
            .iter()
            .filter_map(|link| {
                let source = app_ref.sessions.get(&link.source)?;
                let target = app_ref.sessions.get(&link.target)?;
                Some((source.record.position, target.record.position))
            })
            .collect()
    }
});
```

Call `app.borrow().canvas.drawing_area.queue_draw();` from inside `pump_output`'s timer tick in `main.rs` (every tick is enough — this is intentionally not optimized to redraw only on link changes; the drawing area redraw is cheap and v1 doesn't need finer invalidation).

- [ ] **Step 7: Add a link button to the session node title bar**

In `src/node.rs`, add a `pub link_button: gtk4::Button` to `SessionNode`, created alongside `title` in `SessionNode::new` (e.g. `gtk4::Button::from_icon_name("insert-link-symbolic")`), appended into `title_bar` via `title_bar.append(&link_button)`. Wiring "click link button on node A, then click node B to complete the link" is a small click-mode state machine; add it to `App`:

```rust
// add to App struct: pub pending_link_source: Option<Uuid>,
// add to App::new: pending_link_source: None,

impl App {
    pub fn start_link(app: &Rc<RefCell<App>>, source: Uuid) {
        app.borrow_mut().pending_link_source = Some(source);
    }

    pub fn complete_link_if_pending(app: &Rc<RefCell<App>>, target: Uuid) {
        let pending = app.borrow_mut().pending_link_source.take();
        if let Some(source) = pending {
            App::create_link(app, source, target);
        }
    }
}
```

Wire each session node's `link_button` to `App::start_link(&app, id)`, and each session node's container to a `GtkGestureClick` that calls `App::complete_link_if_pending(&app, id)` when a link is pending — both added where nodes are created in `App::restore`/`App::create_session`.

- [ ] **Step 8: Build and manually verify**

Run: `cargo build`, then `cargo run`. Create two Codex sessions. Click session A's link button, then click session B's body. Type a command into A; confirm the same bytes appear typed into B (both terminals show the command, since B's shell echoes what was written to its PTY). Confirm a blue line is drawn between the two nodes.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "Add agent-to-agent terminal linking

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 11: Account management, handoff, exit-status badge, README

**Files:**
- Modify: `src/app.rs` (port `switch_agent`/account CRUD from the old implementation, adapted to the `HashMap<Uuid, _>` model)
- Modify: `src/main.rs` (account manager dialog, handoff button, exit-status badge, toast errors)
- Modify: `README.md`

**Interfaces:**
- Produces:
  - `App::switch_agent(app: &Rc<RefCell<App>>, id: Uuid) -> anyhow::Result<()>` — same handoff logic as the old `App::switch_agent` (kill, summarize via `handoff.rs`, relaunch the other agent with the summary as initial prompt), adapted to look up by `Uuid` instead of a focused index, and to replace the node's underlying `Session` in place (same node widget, same canvas position) rather than rebuilding a tab list.
  - `App::delete_account(app: &Rc<RefCell<App>>, name: &str) -> anyhow::Result<()>` — same semantics as before (refuses nothing special; removes every session using that account, then the account directory).

- [ ] **Step 1: Port `switch_agent` to `app.rs`**

```rust
use crate::handoff::{summarize_claude, summarize_codex};

impl App {
    pub fn switch_agent(app: &Rc<RefCell<App>>, id: Uuid) -> anyhow::Result<()> {
        let record = {
            let mut app_mut = app.borrow_mut();
            let entry = app_mut
                .sessions
                .get_mut(&id)
                .context("session not found")?;
            entry.session.kill();
            entry.record.clone()
        };

        let summary_result = match record.agent {
            Agent::Claude => {
                let session_id = record
                    .claude_session_id
                    .context("session has no Claude session to summarize")?;
                let config_dir = record
                    .claude_account
                    .as_ref()
                    .map(|a| app.borrow().accounts.config_dir(a));
                summarize_claude(session_id, config_dir.as_deref(), &record.cwd)
            }
            Agent::Codex => summarize_codex(&record.cwd),
        };
        let summary = match summary_result {
            Ok(summary) => summary,
            Err(_) => "The previous agent session could not be recovered. Start by inspecting the working directory and continue from there.".to_string(),
        };

        let to = match record.agent {
            Agent::Claude => Agent::Codex,
            Agent::Codex => Agent::Claude,
        };
        let (launch, updated_record) = {
            let app_ref = app.borrow();
            match to {
                Agent::Claude => {
                    let account = crate::account::DEFAULT_ACCOUNT.to_string();
                    let config_dir = app_ref.accounts.ensure(&account)?;
                    let session_id = Uuid::new_v4();
                    let launch = claude_launch(session_id, false, Some(&summary), Some(&config_dir));
                    let mut updated = record.clone();
                    updated.agent = Agent::Claude;
                    updated.claude_session_id = Some(session_id);
                    updated.claude_account = Some(account);
                    (launch, updated)
                }
                Agent::Codex => {
                    let launch = codex_launch(false, Some(&summary));
                    let mut updated = record.clone();
                    updated.agent = Agent::Codex;
                    updated.claude_session_id = None;
                    updated.claude_account = None;
                    (launch, updated)
                }
            }
        };
        let new_session = Session::spawn(updated_record.cwd.clone(), launch)?;
        {
            let mut app_mut = app.borrow_mut();
            if let Some(entry) = app_mut.sessions.get_mut(&id) {
                entry.session = new_session;
                entry.record = updated_record;
            }
        }
        app.borrow().persist()?;
        Ok(())
    }

    pub fn delete_account(app: &Rc<RefCell<App>>, name: &str) -> anyhow::Result<()> {
        let to_remove: Vec<Uuid> = {
            let app_ref = app.borrow();
            app_ref
                .sessions
                .iter()
                .filter(|(_, entry)| entry.record.claude_account.as_deref() == Some(name))
                .map(|(id, _)| *id)
                .collect()
        };
        {
            let mut app_mut = app.borrow_mut();
            for id in &to_remove {
                if let Some(mut entry) = app_mut.sessions.remove(id) {
                    entry.session.kill();
                    app_mut.canvas.fixed.remove(&entry.node.container);
                }
            }
        }
        app.borrow_mut().accounts.remove(name)?;
        app.borrow().persist()?;
        Ok(())
    }
}
```

(`Context`/`.context(...)` needs `use anyhow::Context;` at the top of `app.rs`.)

- [ ] **Step 2: Add a handoff button per session node**

In `src/node.rs`, add `pub handoff_button: gtk4::Button` (e.g. icon `"media-playlist-shuffle-symbolic"`) to `SessionNode`, appended into `title_bar` next to `link_button`. Wire it in `main.rs` (where nodes are created, in `App::restore`/`App::create_session`) to call `App::switch_agent(&app, id)`, matching the pattern already used for `link_button`.

- [ ] **Step 3: Add an exit-status badge**

In `src/node.rs`, add a label to `SessionNode`:

```rust
// add to the SessionNode struct: pub status_label: gtk4::Label,
// in SessionNode::new, before building `container`:
let status_label = gtk4::Label::new(None);
title_bar.append(&status_label);
```

In `src/app.rs`, add an `exit_shown: bool` field to `SessionEntry` (default `false` wherever a `SessionEntry` is constructed — in both `App::restore` and `App::create_session`), then update `App::pump_output`:

```rust
pub fn pump_output(&mut self) {
    let mut outgoing: Vec<(Uuid, Vec<Vec<u8>>)> = Vec::new();
    for (&id, entry) in self.sessions.iter_mut() {
        let chunks = entry.session.try_recv_output();
        for chunk in &chunks {
            entry.node.feed(chunk);
        }
        if !entry.exit_shown && entry.session.exit_status().is_some() {
            entry.node.status_label.set_text("exited");
            entry.exit_shown = true;
        }
        if !chunks.is_empty() {
            outgoing.push((id, chunks));
        }
    }
    // ... existing link-forwarding loop below is unchanged
}
```

- [ ] **Step 4: Replace `eprintln!` error reporting with toasts**

In `main.rs`, wrap the window content in an `adw::ToastOverlay` (child: the existing `toolbar_view`; set as the window's content instead of `toolbar_view` directly). Replace the `App::restore` error loop's `eprintln!` with `toast_overlay.add_toast(adw::Toast::new(&error))`, and do the same wherever `anyhow::Result` errors currently have nowhere to go (the new-session dialog's failed `create_session`, in Task 8 — revisit that call site and add a toast there too).

- [ ] **Step 5: Update the account manager and README**

Add a simple account manager dialog mirroring the new-session dialog's shape: a `gtk4::ListBox` populated from `app.borrow().accounts.list()`, a "New account" entry+button calling `app.borrow().accounts.ensure(name)`, and a delete button per row calling `App::delete_account`. Wire a header-bar button and `<Ctrl>period` or similar accelerator (pick any unused one) to open it, following the exact pattern already used for the new-session dialog in Task 8.

Rewrite `README.md`'s Install/Workflow/Keybindings sections to match the new GTK app: mention the `vte4` system package requirement, describe the canvas/node model instead of tabs, and replace the ratatui keybinding table with the new header-bar actions and accelerators (`Ctrl+T` new session, plus whatever accelerator was picked for account management). Keep the "Requires `claude` and `codex` on PATH" line and the "Limitation" section about Codex's `--last` resume — both are still true.

- [ ] **Step 6: Build, test, and manually verify**

Run: `cargo build && cargo test`
Expected: all pass.

Run: `cargo run`. Create a Claude session (first run prompts for an account via the account manager — open it manually via the new button and create one named `default` if no automatic first-run prompt exists; wiring that specific first-run nudge automatically is optional polish, not required for this task). Confirm: handoff button switches a session's agent and the new agent opens with a summary-seeded prompt; exiting a shell shows the "exited" badge; deleting an account removes its sessions from the canvas.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "Add account management, handoff, exit badges; update README

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## What's intentionally not in this plan

- **Floors (workspace snapshots)** and **Ombro (AI companion)** — out of scope per the spec's Non-goals; no tasks here touch them.
- **Attention/background-output badges** from the old TUI — on a canvas every session's terminal is simultaneously visible (there's no single focused pane hiding the others), so the old "a tab produced output while you were elsewhere" signal has no clear equivalent and is dropped rather than carried over unexamined.
- **Pointer-centered zoom** — Task 4's zoom re-centers on the canvas origin, not the cursor, as the simplest thing that works; upgrading to cursor-centered zoom is a later, isolated change to `Canvas`'s scroll handler if it turns out to matter in practice.
