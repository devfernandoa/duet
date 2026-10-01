//! A GTK widget that displays a terminal. Has zero PTY/process-management
//! knowledge (that's `session.rs`'s job) and zero knowledge of canvas
//! positioning (that's `canvas.rs`'s job) — it only knows how to display
//! bytes (`feed`) and report what the user typed (`connect_commit`).

use gtk4::prelude::*;
use vte4::TerminalExt;

/// Minimum size (pixels, pre-zoom) either a session card's terminal or a
/// note's text area can be resized down to via `resize_handle`.
pub const MIN_NODE_WIDTH: f64 = 220.0;
pub const MIN_NODE_HEIGHT: f64 = 140.0;

/// Pixel size -> terminal character grid, clamped to what a PTY accepts
/// (`Session::resize` floors at 2 columns / 1 row). Split out from
/// `SessionNode::resize_grid` so the arithmetic is testable without a GTK
/// display. `None` when the cell size is non-positive, which would otherwise
/// turn a pixel width straight into an absurd column count.
fn grid_size(width: f64, height: f64, char_width: i64, char_height: i64) -> Option<(u16, u16)> {
    if char_width <= 0 || char_height <= 0 || !width.is_finite() || !height.is_finite() {
        return None;
    }
    let fit = |pixels: f64, cell: i64, floor: f64| {
        (pixels / cell as f64).floor().clamp(floor, u16::MAX as f64) as u16
    };
    Some((
        fit(width, char_width, 2.0),
        fit(height, char_height, 1.0),
    ))
}

/// Builds the small bottom-right resize grip shared by `SessionNode` and
/// `NoteNode`: a plain `Box` overlaid on the node's content widget. Callers
/// wire a `GestureDrag` onto it (in `app.rs`, which owns canvas/position
/// knowledge) and apply `nwse-resize` cursor affordance here, since that's
/// purely cosmetic widget setup.
fn resize_handle() -> gtk4::Box {
    let handle = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    handle.set_size_request(14, 14);
    handle.set_halign(gtk4::Align::End);
    handle.set_valign(gtk4::Align::End);
    handle.add_css_class("resize-handle");
    handle.set_cursor_from_name(Some("nwse-resize"));
    handle
}

pub struct SessionNode {
    pub container: gtk4::Box,
    pub title_bar: gtk4::Box,
    pub terminal: vte4::Terminal,
    pub link_button: gtk4::Button,
    pub handoff_button: gtk4::Button,
    pub status_label: gtk4::Label,
    /// Drag this to move the card on the canvas (wired in `app.rs`). An
    /// empty, hexpanding spacer between the title and the action buttons, so
    /// dragging it never races with clicking `link_button`/`handoff_button`/
    /// `close_button` — those are siblings under `title_bar`, not inside
    /// `drag_handle`, so a press on them is never seen by the drag gesture
    /// attached to `drag_handle` (same reasoning as `wire_link_controls`'s
    /// doc comment about attaching to specific widgets, not shared ancestors).
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    /// Drag this to resize the terminal (wired in `app.rs`).
    pub resize_handle: gtk4::Box,
}

impl SessionNode {
    pub fn new(name: &str) -> SessionNode {
        let title = gtk4::Label::new(Some(name));
        title.add_css_class("heading");

        let drag_handle = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        drag_handle.set_hexpand(true);
        drag_handle.set_cursor_from_name(Some("grab"));

        let link_button = gtk4::Button::from_icon_name("insert-link-symbolic");
        let handoff_button = gtk4::Button::from_icon_name("media-playlist-shuffle-symbolic");
        let status_label = gtk4::Label::new(None);
        let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_button.add_css_class("flat");

        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title_bar.add_css_class("node-title-bar");
        title_bar.append(&title);
        title_bar.append(&drag_handle);
        title_bar.append(&status_label);
        title_bar.append(&link_button);
        title_bar.append(&handoff_button);
        title_bar.append(&close_button);

        // Deliberately no `set_size_request` here: a VTE terminal's size is
        // its character grid, and `set_size_request` only raises the widget's
        // *minimum*. VTE's natural size (80x24 grid = 722x506px at a 9x21
        // cell) stayed larger than any request under ~722x506, so `GtkFixed`
        // kept allocating the natural size and the request had no effect at
        // all. Callers size the terminal through `resize_grid` instead.
        let terminal = vte4::Terminal::new();

        let resize_handle = resize_handle();
        let body = gtk4::Overlay::new();
        body.set_child(Some(&terminal));
        body.add_overlay(&resize_handle);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&title_bar);
        container.append(&body);
        container.add_css_class("card");

        SessionNode {
            container,
            title_bar,
            terminal,
            link_button,
            handoff_button,
            status_label,
            drag_handle,
            close_button,
            resize_handle,
        }
    }

    /// Resizes the terminal to the largest character grid that fits
    /// `width` x `height` pixels, and reports the new grid so the caller can
    /// resize the PTY to match via `Session::resize`.
    ///
    /// Why the grid and not pixels: a `vte4::Terminal` renders a fixed
    /// character grid, and *two* separate things have to be told about a
    /// resize. VTE recomputes its own grid from whatever allocation it gets,
    /// but the PTY it is displaying does not — it keeps whatever size
    /// `Session::spawn` opened it with (80x24), so the program inside keeps
    /// wrapping its output at 80 columns while VTE renders a wider grid.
    /// That mismatch is what garbled the text on resize.
    ///
    /// Returns `None` when the grid is already that size (so the caller skips
    /// a redundant PTY resize — this also throttles `SIGWINCH` during a drag
    /// to once per character cell crossed, rather than once per motion
    /// event), or when VTE reports a non-positive cell size and the
    /// conversion would be meaningless.
    pub fn resize_grid(&self, width: f64, height: f64) -> Option<(u16, u16)> {
        let (cols, rows) = grid_size(
            width,
            height,
            self.terminal.char_width(),
            self.terminal.char_height(),
        )?;
        if (i64::from(cols), i64::from(rows))
            == (self.terminal.column_count(), self.terminal.row_count())
        {
            return None;
        }
        self.terminal.set_size(i64::from(cols), i64::from(rows));
        Some((cols, rows))
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

/// A sticky note on the canvas: a plain text view in a color-tinted card, with
/// no PTY/process knowledge (unlike `SessionNode`). Has the same title-bar
/// chrome (drag handle, close button) and resize handle as `SessionNode`, so
/// notes and session cards behave the same way on the canvas.
pub struct NoteNode {
    pub container: gtk4::Box,
    pub title_bar: gtk4::Box,
    pub text_view: gtk4::TextView,
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    pub resize_handle: gtk4::Box,
}

impl NoteNode {
    pub fn new(initial_text: &str, color: &str) -> NoteNode {
        let drag_handle = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        drag_handle.set_hexpand(true);
        drag_handle.set_cursor_from_name(Some("grab"));

        let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_button.add_css_class("flat");

        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title_bar.add_css_class("note-title-bar");
        title_bar.append(&drag_handle);
        title_bar.append(&close_button);

        let text_view = gtk4::TextView::new();
        text_view.buffer().set_text(initial_text);
        text_view.set_wrap_mode(gtk4::WrapMode::Word);
        text_view.set_size_request(220, 160);
        text_view.set_top_margin(8);
        text_view.set_bottom_margin(8);
        text_view.set_left_margin(8);
        text_view.set_right_margin(8);
        // Forces dark, legible text regardless of the system theme. See the
        // `textview.note-text` rule in style.css for why the `color` has to
        // sit on this node rather than on the text view's internal `text`
        // node. The pastel background is set on both nodes, since a
        // GtkTextView otherwise paints its own theme-default background over
        // whatever color the card behind it sets.
        text_view.add_css_class("note-text");

        let resize_handle = resize_handle();
        let body = gtk4::Overlay::new();
        body.set_child(Some(&text_view));
        body.add_overlay(&resize_handle);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&title_bar);
        container.append(&body);
        container.set_css_classes(&["card", &format!("note-{color}")]);

        NoteNode {
            container,
            title_bar,
            text_view,
            drag_handle,
            close_button,
            resize_handle,
        }
    }

    pub fn text(&self) -> String {
        let buffer = self.text_view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::grid_size;

    #[test]
    fn grid_size_floors_to_whole_cells() {
        // 9x21 is VTE's cell size at this app's default font.
        assert_eq!(grid_size(722.0, 506.0, 9, 21), Some((80, 24)));
        // A partial trailing cell is dropped, never rounded up: a column the
        // PTY reports but VTE cannot fully draw is what misaligns the text.
        assert_eq!(grid_size(728.0, 524.0, 9, 21), Some((80, 24)));
    }

    #[test]
    fn grid_size_clamps_to_what_a_pty_accepts() {
        assert_eq!(grid_size(0.0, 0.0, 9, 21), Some((2, 1)));
    }

    #[test]
    fn grid_size_rejects_unusable_cell_metrics() {
        assert_eq!(grid_size(722.0, 506.0, 0, 21), None);
        assert_eq!(grid_size(f64::NAN, 506.0, 9, 21), None);
    }
}

