//! A GTK widget that displays a terminal. Has zero PTY/process-management
//! knowledge (that's `session.rs`'s job) and zero knowledge of canvas
//! positioning (that's `canvas.rs`'s job) — it only knows how to display
//! bytes (`feed`) and report what the user typed (`connect_commit`).

use gtk4::prelude::*;
use vte4::TerminalExt;

/// Minimum size (pixels, pre-zoom) either a session card's terminal or a
/// note's text area can be resized down to via `resize_handle`. This floor is
/// what keeps the resize grip reachable: at 220x140 the grip still sits on a
/// body large enough to hold it clear of the title bar.
pub const MIN_NODE_WIDTH: f64 = 220.0;
pub const MIN_NODE_HEIGHT: f64 = 140.0;

/// Character-grid floor, independent of the pixel floor above. The pixel
/// floor normally dominates (220x140 is a 24x6 grid at a 9x21 cell), but a
/// large font or a high-DPI cell size could make MIN_NODE_* map to a
/// degenerate grid, and a 2x1 terminal is worse than useless — it makes
/// almost any program's output unreadable and `SIGWINCH`-thrashes the agent.
const MIN_GRID_COLS: f64 = 10.0;
const MIN_GRID_ROWS: f64 = 3.0;

/// Pixel size -> terminal character grid, floored at `MIN_GRID_COLS` x
/// `MIN_GRID_ROWS`. Split out from `SessionNode::request_grid` so the
/// arithmetic is testable without a GTK display. `None` when the cell size is
/// non-positive, which would otherwise turn a pixel width straight into an
/// absurd column count.
fn grid_size(width: f64, height: f64, char_width: i64, char_height: i64) -> Option<(u16, u16)> {
    if char_width <= 0 || char_height <= 0 || !width.is_finite() || !height.is_finite() {
        return None;
    }
    let fit = |pixels: f64, cell: i64, floor: f64| {
        (pixels / cell as f64).floor().clamp(floor, u16::MAX as f64) as u16
    };
    Some((
        fit(width, char_width, MIN_GRID_COLS),
        fit(height, char_height, MIN_GRID_ROWS),
    ))
}

/// Builds the small bottom-right resize grip shared by `SessionNode` and
/// `NoteNode`: a plain `Box` overlaid on the node's content widget. Callers
/// wire a `GestureDrag` onto it (in `app.rs`, which owns canvas/position
/// knowledge) and apply `nwse-resize` cursor affordance here, since that's
/// purely cosmetic widget setup.
fn resize_handle() -> gtk4::Box {
    let handle = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    // 16px rather than 14: this is the only way to resize a card, and at the
    // minimum card size it is the only chrome on the body at all, so it needs
    // a hit area that is comfortable to find without zooming in.
    handle.set_size_request(16, 16);
    handle.set_halign(gtk4::Align::End);
    handle.set_valign(gtk4::Align::End);
    handle.add_css_class("resize-handle");
    handle.set_cursor_from_name(Some("nwse-resize"));
    handle
}

/// Shows a session card's rename entry (pre-filled and focused) in place of
/// its title label, or puts the label back. A free function taking both
/// widgets rather than a `SessionNode` method so `app.rs`'s rename wiring can
/// call it from inside signal handlers that only captured the two widgets —
/// and so "exactly one of the two is visible" lives in one place either way.
pub fn set_renaming(label: &gtk4::Label, entry: &gtk4::Entry, renaming: bool) {
    if renaming {
        entry.set_text(&label.text());
    }
    label.set_visible(!renaming);
    entry.set_visible(renaming);
    if renaming {
        entry.grab_focus();
    }
}

/// Collapse/expand a node to just its title bar. Deliberately self-contained
/// here rather than wired from `app.rs`: it is pure widget visibility with no
/// bearing on the session, its PTY, or the store. Not persisted — a collapsed
/// card comes back expanded, which `store.rs` would need a new field to
/// change and nobody asked for that.
fn wire_minimize(button: &gtk4::Button, body: &gtk4::Overlay) {
    let sync = |button: &gtk4::Button, expanded: bool| {
        button.set_icon_name(if expanded {
            "go-up-symbolic"
        } else {
            "go-down-symbolic"
        });
        button.set_tooltip_text(Some(if expanded {
            "Collapse to title bar"
        } else {
            "Expand"
        }));
    };
    sync(button, true);
    button.connect_clicked({
        let body = body.clone();
        move |button| {
            let expanded = !body.is_visible();
            body.set_visible(expanded);
            sync(button, expanded);
        }
    });
}

/// Builds the role badge shown in a session card's title bar: an icon plus
/// the role's name, hidden until `SessionNode::set_role` is given a role.
/// A plain function (not inlined into `SessionNode::new`) so the three
/// widgets it returns can be stored as fields without repeating the
/// construction.
fn role_badge_widgets() -> (gtk4::Box, gtk4::Image, gtk4::Label) {
    let icon = gtk4::Image::new();
    icon.set_visible(false);
    let label = gtk4::Label::new(None);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_max_width_chars(10);
    let badge = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    badge.add_css_class("role-badge");
    badge.append(&icon);
    badge.append(&label);
    badge.set_visible(false);
    (badge, icon, label)
}

pub struct SessionNode {
    pub container: gtk4::Box,
    /// The session's name, shown when not being renamed. Click it to rename
    /// (wired in `app.rs`, which owns the uniqueness rule and persistence).
    pub title_label: gtk4::Label,
    /// Swapped in for `title_label` during an inline rename. Exactly one of
    /// the two is visible at a time; see `SessionNode::set_renaming`.
    pub title_entry: gtk4::Entry,
    role_badge: gtk4::Box,
    role_icon: gtk4::Image,
    role_label: gtk4::Label,
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
        let title_label = gtk4::Label::new(Some(name));
        title_label.add_css_class("heading");
        title_label.add_css_class("node-title");
        title_label.set_cursor_from_name(Some("pointer"));
        // A long session name otherwise sets the whole card's minimum width
        // (a 28-character name measured 348px wide), so the card could not be
        // resized narrower than its own title and `record.size` drifted away
        // from the card's real width.
        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title_label.set_max_width_chars(16);

        let title_entry = gtk4::Entry::new();
        title_entry.set_visible(false);
        title_entry.set_max_width_chars(16);

        let drag_handle = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        drag_handle.set_hexpand(true);
        drag_handle.set_cursor_from_name(Some("grab"));

        let minimize_button = gtk4::Button::from_icon_name("go-up-symbolic");
        minimize_button.add_css_class("flat");
        let link_button = gtk4::Button::from_icon_name("insert-link-symbolic");
        link_button.add_css_class("flat");
        link_button.set_tooltip_text(Some("Link this session's output into another"));
        let handoff_button = gtk4::Button::from_icon_name("media-playlist-shuffle-symbolic");
        handoff_button.add_css_class("flat");
        handoff_button.set_tooltip_text(Some("Hand off to the other agent"));
        let status_label = gtk4::Label::new(None);
        let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_button.add_css_class("flat");
        close_button.set_tooltip_text(Some("Close session"));

        let (role_badge, role_icon, role_label) = role_badge_widgets();

        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title_bar.add_css_class("node-title-bar");
        title_bar.append(&title_label);
        title_bar.append(&title_entry);
        title_bar.append(&role_badge);
        title_bar.append(&drag_handle);
        title_bar.append(&status_label);
        title_bar.append(&minimize_button);
        title_bar.append(&link_button);
        title_bar.append(&handoff_button);
        title_bar.append(&close_button);

        // No size request here, but only because callers immediately set one
        // via `request_grid` (restore/create from the record, resize drags
        // from the pointer). Without one a card is allocated VTE's *minimum*
        // — one row tall — because `GtkFixed` allocates minimums, not natural
        // sizes; see `request_grid`.
        let terminal = vte4::Terminal::new();

        // Focus-follows-mouse ("sloppy focus"): hovering a card's terminal is
        // enough to start typing into it, no click needed. An
        // `EventControllerMotion` is deliberately used rather than a click
        // gesture — it only observes crossing/motion events and never claims
        // a button sequence, so it cannot race the drag/resize `GestureDrag`s
        // or the link-completion `GestureClick` that also live on this card.
        let motion = gtk4::EventControllerMotion::new();
        motion.connect_enter({
            let terminal = terminal.clone();
            let title_entry = title_entry.clone();
            move |_controller, _x, _y| {
                // One case where hovering must *not* take focus: an inline
                // rename in progress. The entry sits a few pixels above the
                // terminal, so drifting the pointer down while typing a new
                // name would otherwise throw the keystrokes at the agent.
                if WidgetExt::is_visible(&title_entry) {
                    return;
                }
                terminal.grab_focus();
            }
        });
        terminal.add_controller(motion);

        let resize_handle = resize_handle();
        let body = gtk4::Overlay::new();
        body.set_child(Some(&terminal));
        body.add_overlay(&resize_handle);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&title_bar);
        container.append(&body);
        container.add_css_class("card");

        wire_minimize(&minimize_button, &body);

        let node = SessionNode {
            container,
            title_label,
            title_entry,
            role_badge,
            role_icon,
            role_label,
            terminal,
            link_button,
            handoff_button,
            status_label,
            drag_handle,
            close_button,
            resize_handle,
        };
        node.set_name(name);
        node
    }

    /// Sets the displayed name. The label is ellipsized (so a long name
    /// can't set the card's minimum width — see `new`), which is why the
    /// full name also goes in the tooltip, together with the hint that
    /// clicking the label renames the session.
    pub fn set_name(&self, name: &str) {
        self.title_label.set_text(name);
        self.title_label
            .set_tooltip_text(Some(&format!("{name}\nClick to rename")));
    }

    /// Shows the role badge (icon + name) in the title bar, or hides it when
    /// `name` is `None`. Takes plain strings rather than a `role::Role`, the
    /// same way `new` takes a plain `&str` name rather than a
    /// `SessionRecord` — this module stays ignorant of the domain record
    /// types, which live in `store.rs`/`role.rs` and are resolved by `app.rs`.
    pub fn set_role(&self, name: Option<&str>, icon: Option<&str>, accent: Option<&str>) {
        for accent_name in crate::role::ACCENTS {
            self.role_badge
                .remove_css_class(&format!("role-accent-{accent_name}"));
        }
        let Some(name) = name else {
            self.role_badge.set_visible(false);
            return;
        };
        self.role_label.set_text(name);
        self.role_icon.set_icon_name(icon);
        self.role_icon.set_visible(icon.is_some());
        if let Some(accent) = accent {
            self.role_badge
                .add_css_class(&format!("role-accent-{accent}"));
        }
        self.role_badge.set_tooltip_text(Some(name));
        self.role_badge.set_visible(true);
    }

    /// Sizes the terminal to `width` x `height` pixels, rounded down to a
    /// whole character grid of at least `MIN_GRID_COLS` x `MIN_GRID_ROWS`.
    /// A no-op when VTE reports a non-positive cell size (the pixel -> grid
    /// conversion would be meaningless).
    ///
    /// It has to be `set_size_request`, i.e. the widget's *minimum*, and not
    /// `vte_terminal_set_size`, i.e. its character grid. `GtkFixed` allocates
    /// every child at its **minimum** size, never its natural size — measured
    /// on a real card: `container: min=(155x49) nat=(722x532) alloc=(155x49)`.
    /// VTE's grid only feeds its *natural* size, so `set_size` moved a number
    /// nothing downstream ever read: every resize tick set the grid, the next
    /// allocation recomputed it straight back from the unchanged 153x21
    /// minimum allocation, and a full drag ended with the card the exact size
    /// it started. That is why resize appeared completely dead rather than
    /// merely wrong. A note's text view was always sized this way
    /// (`set_size_request` in `app.rs`), which is why notes resized and cards
    /// did not.
    ///
    /// Nothing here touches the PTY: the request is a floor, so the title
    /// bar's own minimum width can still overrule a narrower one and leave
    /// the terminal wider than asked. `App::pump_output` syncs the PTY from
    /// `column_count()`/`row_count()` after allocation, the only grid numbers
    /// that are actually true; telling the PTY what was *requested* instead
    /// is what garbled the text of a card narrower than its title bar.
    pub fn request_grid(&self, width: f64, height: f64) {
        let (char_width, char_height) = (self.terminal.char_width(), self.terminal.char_height());
        let Some((cols, rows)) = grid_size(width, height, char_width, char_height) else {
            return;
        };
        // `grid_size` floors the grid, so when that floor bites (a drag below
        // MIN_GRID_*) the grid's pixel size is the larger of the two.
        self.terminal.set_size_request(
            width.max(f64::from(cols) * char_width as f64) as i32,
            height.max(f64::from(rows) * char_height as f64) as i32,
        );
    }

    /// The terminal's real character grid as `(cols, rows)` — what VTE is
    /// actually rendering after allocation, which is what the PTY must match.
    /// `None` while the terminal isn't mapped (a collapsed card, or before
    /// first allocation), since a hidden widget's grid says nothing about
    /// what the program inside should wrap to.
    pub fn actual_grid(&self) -> Option<(u16, u16)> {
        if !self.terminal.is_mapped() {
            return None;
        }
        let cols = u16::try_from(self.terminal.column_count()).ok()?;
        let rows = u16::try_from(self.terminal.row_count()).ok()?;
        (f64::from(cols) >= MIN_GRID_COLS && f64::from(rows) >= MIN_GRID_ROWS)
            .then_some((cols, rows))
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

        let minimize_button = gtk4::Button::from_icon_name("go-up-symbolic");
        minimize_button.add_css_class("flat");
        let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_button.add_css_class("flat");
        close_button.set_tooltip_text(Some("Delete note"));

        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title_bar.add_css_class("note-title-bar");
        title_bar.append(&drag_handle);
        title_bar.append(&minimize_button);
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

        wire_minimize(&minimize_button, &body);

        NoteNode {
            container,
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

    /// The floor is what stops a card from reaching a degenerate terminal
    /// state it can't usefully be dragged back out of.
    #[test]
    fn grid_size_never_goes_below_the_usable_floor() {
        assert_eq!(grid_size(0.0, 0.0, 9, 21), Some((10, 3)));
        // A cell size big enough that even MIN_NODE_* maps below the floor.
        assert_eq!(
            grid_size(super::MIN_NODE_WIDTH, super::MIN_NODE_HEIGHT, 40, 80),
            Some((10, 3))
        );
    }

    #[test]
    fn grid_size_rejects_unusable_cell_metrics() {
        assert_eq!(grid_size(722.0, 506.0, 0, 21), None);
        assert_eq!(grid_size(f64::NAN, 506.0, 9, 21), None);
    }

    /// The regression that made resize look *completely* dead rather than
    /// merely wrong: `GtkFixed` allocates children at their MINIMUM size, so
    /// sizing a card through VTE's character grid — which only feeds its
    /// *natural* size — moved a number nothing downstream ever read. No pure
    /// function can catch that; it takes a real allocation. Needs a display,
    /// so: `cargo test -- --ignored --test-threads=1`.
    #[test]
    #[ignore = "needs a display"]
    fn request_grid_drives_the_cards_real_allocation() {
        use gtk4::prelude::*;
        if gtk4::init().is_err() {
            return;
        }
        let node = super::SessionNode::new("probe");
        let fixed = gtk4::Fixed::new();
        fixed.put(&node.container, 0.0, 0.0);
        let window = gtk4::Window::new();
        window.set_default_size(1600, 1200);
        window.set_child(Some(&fixed));
        node.request_grid(600.0, 400.0);
        window.present();

        let context = gtk4::glib::MainContext::default();
        for _ in 0..400 {
            while context.pending() {
                context.iteration(false);
            }
            if node.terminal.width() > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (width, height) = (node.terminal.width(), node.terminal.height());
        window.destroy();
        // Within one character cell of what was asked for: the request is
        // floored to whole cells, and VTE's own padding costs a pixel or two.
        assert!(
            (width - 600).abs() <= 12 && (height - 400).abs() <= 24,
            "terminal allocated {width}x{height}, wanted ~600x400"
        );
    }
}
