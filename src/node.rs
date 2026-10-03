//! GTK widgets that display one canvas node each. These widgets know how to
//! show bytes/text and report user input/edits; they have zero PTY/process
//! knowledge (`session.rs`/`runtime.rs`'s job) and zero canvas positioning
//! knowledge (`canvas.rs`'s job), and zero knowledge of `NodeRecord`/
//! `model.rs` types beyond the plain strings/bools `app.rs` passes in.

use gtk4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use vte4::TerminalExt;

/// Minimum size (pixels, pre-zoom) any card can be resized down to via
/// `resize_handle`. This floor is what keeps the resize grip reachable: at
/// 220x140 the grip still sits on a body large enough to hold it clear of
/// the title bar.
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

/// Builds the small bottom-right resize grip shared by every node kind: a
/// plain `Box` overlaid on the node's content widget. Callers wire a
/// `GestureDrag` onto it (in `app.rs`, which owns canvas/position knowledge)
/// and apply `nwse-resize` cursor affordance here, since that's purely
/// cosmetic widget setup.
pub(crate) fn resize_handle() -> gtk4::Box {
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

fn sync_minimize_icon(button: &gtk4::Button, expanded: bool) {
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
}

/// A handle onto one node's collapse/expand widget state, returned by
/// `wire_minimize` and stored on every node struct so `app.rs`'s "Collapse
/// Selected"/"Expand Selected" menu commands — and undo/redo of a collapse
/// toggle — can drive the same visibility the minimize button itself
/// controls. Before this existed, only clicking the button (through
/// `wire_minimize`'s own closure) could ever change `body`'s visibility, so
/// `NodeRecord::collapsed` and what was actually on screen could silently
/// disagree.
#[derive(Clone)]
pub struct CollapseHandle {
    button: gtk4::Button,
    body: gtk4::Overlay,
    /// The whole card. Cards sized through their container (every kind but
    /// a terminal, whose size lives on the VTE widget inside `body`) keep
    /// their height request here only while expanded — otherwise hiding
    /// `body` couldn't shrink the card to its title bar.
    card: gtk4::Widget,
    /// The card size last requested through `set_card_size`, re-applied on
    /// expand. `None` for a card never sized that way.
    card_size: Rc<Cell<Option<(i32, i32)>>>,
}

impl CollapseHandle {
    /// Applies `collapsed` to the widget if it isn't already in that state.
    /// Idempotent and cheap to call unconditionally (e.g. from undo/redo
    /// replaying a `SetProperties` command that didn't actually touch
    /// `collapsed`), since it no-ops when nothing needs to change.
    pub fn set_collapsed(&self, collapsed: bool) {
        let expanded = !collapsed;
        if self.body.is_visible() != expanded {
            self.body.set_visible(expanded);
            sync_minimize_icon(&self.button, expanded);
            self.apply_card_size();
        }
    }

    pub fn is_collapsed(&self) -> bool {
        !self.body.is_visible()
    }

    /// Sizes the whole card (title bar included). While collapsed only the
    /// width applies; the height comes back on expand.
    pub fn set_card_size(&self, width: i32, height: i32) {
        self.card_size.set(Some((width, height)));
        self.apply_card_size();
    }

    fn apply_card_size(&self) {
        let Some((width, height)) = self.card_size.get() else {
            return;
        };
        let height = if self.body.is_visible() { height } else { -1 };
        self.card.set_size_request(width, height);
    }
}

/// Collapse/expand a node to just its title bar, via its minimize button.
/// Also returns a `CollapseHandle` for driving the same state
/// programmatically (see its doc comment). Deliberately self-contained here
/// rather than wired from `app.rs`: it is pure widget visibility with no
/// bearing on the node's record, its runtime, or the store caller side —
/// `app.rs` separately persists `NodeRecord::collapsed` so the state survives
/// a restart; `on_toggle` is how a direct button click reports that back.
pub(crate) fn wire_minimize(
    button: &gtk4::Button,
    body: &gtk4::Overlay,
    card: &impl IsA<gtk4::Widget>,
    initially_collapsed: bool,
    on_toggle: impl Fn(bool) + 'static,
) -> CollapseHandle {
    let expanded = !initially_collapsed;
    body.set_visible(expanded);
    sync_minimize_icon(button, expanded);
    let handle = CollapseHandle {
        button: button.clone(),
        body: body.clone(),
        card: card.clone().upcast(),
        card_size: Rc::new(Cell::new(None)),
    };
    button.connect_clicked({
        let handle = handle.clone();
        move |_| {
            let collapsed = !handle.is_collapsed();
            handle.set_collapsed(collapsed);
            on_toggle(collapsed);
        }
    });
    handle
}

/// The compact title-bar buttons every card keeps: collapse and close.
/// Everything else a card can do lives in its right-click menu (wired in
/// `app.rs`), so the title bar stays readable at any card width.
fn title_buttons(close_tooltip: &str) -> (gtk4::Button, gtk4::Button) {
    let minimize_button = gtk4::Button::from_icon_name("go-up-symbolic");
    minimize_button.add_css_class("flat");
    let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
    close_button.add_css_class("flat");
    close_button.set_tooltip_text(Some(close_tooltip));
    (minimize_button, close_button)
}

/// An empty, expanding spacer for a title bar.
fn spacer() -> gtk4::Box {
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    spacer
}

/// Marks `title_bar` as the card's drag handle: the whole bar (title text
/// included) moves the card, and it says so on hover.
pub(crate) fn as_drag_handle(title_bar: &gtk4::Box) {
    title_bar.set_cursor_from_name(Some("grab"));
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

/// The full text of a `GtkTextBuffer`, for reading back whatever the user
/// typed/edited — shared by every text-bearing node kind below, and by
/// `app.rs`'s own buffer-changed handlers (which already have the buffer in
/// hand as the signal's own argument, so they can read it directly instead
/// of re-borrowing `App` to look the widget back up).
pub(crate) fn buffer_text(buffer: &gtk4::TextBuffer) -> String {
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
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
    pub status_label: gtk4::Label,
    /// The whole title bar: dragging anywhere on it (the name included)
    /// moves the card, and right-clicking it opens the card's menu (both
    /// wired in `app.rs`). Its buttons still get their own clicks — the
    /// move only starts once the pointer actually travels.
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    /// Drag this to resize the terminal (wired in `app.rs`).
    pub resize_handle: gtk4::Box,
    /// Lets `app.rs` collapse/expand this card from outside a button click —
    /// see `CollapseHandle`'s doc comment.
    pub collapse: CollapseHandle,
}

impl SessionNode {
    pub fn new(
        name: &str,
        collapsed: bool,
        on_collapse_toggle: impl Fn(bool) + 'static,
    ) -> SessionNode {
        let title_label = gtk4::Label::new(Some(name));
        title_label.add_css_class("heading");
        title_label.add_css_class("node-title");
        // A long session name otherwise sets the whole card's minimum width
        // (a 28-character name measured 348px wide), so the card could not be
        // resized narrower than its own title and `record.size` drifted away
        // from the card's real width.
        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title_label.set_max_width_chars(16);

        let title_entry = gtk4::Entry::new();
        title_entry.set_visible(false);
        title_entry.set_max_width_chars(16);

        let status_label = gtk4::Label::new(None);
        status_label.add_css_class("dim-label");
        let (minimize_button, close_button) = title_buttons("Close session");

        let (role_badge, role_icon, role_label) = role_badge_widgets();

        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title_bar.add_css_class("node-title-bar");
        title_bar.append(&title_label);
        title_bar.append(&title_entry);
        title_bar.append(&role_badge);
        title_bar.append(&spacer());
        title_bar.append(&status_label);
        title_bar.append(&minimize_button);
        title_bar.append(&close_button);
        as_drag_handle(&title_bar);
        let drag_handle = title_bar.clone();

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

        let collapse = wire_minimize(
            &minimize_button,
            &body,
            &container,
            collapsed,
            on_collapse_toggle,
        );

        let node = SessionNode {
            container,
            title_label,
            title_entry,
            role_badge,
            role_icon,
            role_label,
            terminal,
            status_label,
            drag_handle,
            close_button,
            resize_handle,
            collapse,
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
            .set_tooltip_text(Some(&format!("{name}\nDouble-click to rename")));
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

/// A Markdown note on the canvas: Edit mode shows raw Markdown source in an
/// editable `GtkTextView`; Preview renders it (via `markdown.rs`) into a
/// second, read-only `GtkTextView`; Split shows both side by side in a
/// `GtkPaned`. All three modes are implemented as one `GtkPaned` whose two
/// children are simply shown/hidden (`set_view_mode`) rather than reparented
/// between a `GtkStack`'s pages and the `GtkPaned` — reparenting a live GTK
/// widget between containers on every mode switch would be the "large amount
/// of special-case GTK code" the Split-mode requirement explicitly allows
/// skipping; toggling visibility on two permanently-parented children avoids
/// that entirely. Plain Markdown source editing (Edit mode) is always
/// available, satisfying that same requirement.
#[derive(Clone)]
pub struct NoteNode {
    pub container: gtk4::Box,
    pub edit_view: gtk4::TextView,
    /// Kept as a field (not just a closure-captured local) so
    /// `refresh_preview_after_resize` can re-render into it after a resize,
    /// not only after an edit.
    preview_view: gtk4::TextView,
    /// `true` while a `refresh_preview_after_resize`-queued idle callback is
    /// still pending — see that method's doc comment for why this debounces
    /// multiple calls (one per resize-drag tick) into at most one callback.
    preview_refresh_scheduled: Rc<Cell<bool>>,
    edit_scroller: gtk4::ScrolledWindow,
    preview_scroller: gtk4::ScrolledWindow,
    /// The "link this note to a project file" popover (opened from the
    /// card's menu, wired in `app::files`), parented to the title bar once
    /// and reused — unparenting a closed popover crashes GTK 4.14 (see
    /// `node_files::FileTreeNode::menu_popover`).
    pub file_popover: gtk4::Popover,
    /// The note's own title (its first heading or line), so a collapsed
    /// note still says what it is.
    title_label: gtk4::Label,
    /// The backing file's path, shown only for a file-backed note.
    file_label: gtk4::Label,
    /// A one-word sync state ("synced", "conflict", ...) for a file-backed
    /// note.
    sync_label: gtk4::Label,
    /// Shown when the backing file and this note disagree in a way that
    /// needs the user to choose (see `project::sync::reconcile`).
    banner: gtk4::Box,
    banner_label: gtk4::Label,
    pub banner_primary: gtk4::Button,
    pub banner_secondary: gtk4::Button,
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    pub resize_handle: gtk4::Box,
    pub collapse: CollapseHandle,
}

impl NoteNode {
    pub fn new(
        initial_markdown: &str,
        color: &str,
        initial_mode: crate::model::NoteViewMode,
        collapsed: bool,
        on_collapse_toggle: impl Fn(bool) + 'static,
    ) -> NoteNode {
        let (minimize_button, close_button) = title_buttons("Delete note");
        let title_label = gtk4::Label::new(None);
        title_label.add_css_class("note-title");
        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title_label.set_xalign(0.0);
        let file_label = gtk4::Label::new(None);
        file_label.add_css_class("note-file-label");
        file_label.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
        file_label.set_width_chars(8);
        file_label.set_max_width_chars(18);
        file_label.set_visible(false);
        let sync_label = gtk4::Label::new(None);
        sync_label.add_css_class("note-file-label");
        sync_label.set_visible(false);

        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title_bar.add_css_class("note-title-bar");
        title_bar.append(&title_label);
        title_bar.append(&spacer());
        title_bar.append(&file_label);
        title_bar.append(&sync_label);
        title_bar.append(&minimize_button);
        title_bar.append(&close_button);
        as_drag_handle(&title_bar);
        let drag_handle = title_bar.clone();
        let file_popover = gtk4::Popover::new();
        file_popover.set_parent(&title_bar);

        let edit_view = gtk4::TextView::new();
        edit_view.buffer().set_text(initial_markdown);
        edit_view.set_wrap_mode(gtk4::WrapMode::Word);
        edit_view.set_top_margin(8);
        edit_view.set_bottom_margin(8);
        edit_view.set_left_margin(8);
        edit_view.set_right_margin(8);
        // Forces dark, legible text regardless of the system theme. See the
        // `textview.note-text` rule in style.css for why the `color` has to
        // sit on this node rather than on the text view's internal `text`
        // node. The pastel background is set on both nodes, since a
        // GtkTextView otherwise paints its own theme-default background over
        // whatever color the card behind it sets.
        edit_view.add_css_class("note-text");

        let preview_view = gtk4::TextView::new();
        preview_view.set_editable(false);
        preview_view.set_cursor_visible(false);
        preview_view.set_wrap_mode(gtk4::WrapMode::Word);
        preview_view.set_top_margin(8);
        preview_view.set_bottom_margin(8);
        preview_view.set_left_margin(8);
        preview_view.set_right_margin(8);
        preview_view.add_css_class("note-text");
        crate::markdown::render_to_buffer(&preview_view, &crate::markdown::parse(initial_markdown));

        // Keeps Preview (and the visible half of Split) live as the user
        // types in Edit, rather than only re-rendering on a mode switch.
        edit_view.buffer().connect_changed({
            let preview_view = preview_view.clone();
            move |buffer| {
                let source = buffer_text(buffer);
                crate::markdown::render_to_buffer(&preview_view, &crate::markdown::parse(&source));
            }
        });

        // `vexpand` (to match the `hexpand` already set below) is what makes
        // the card's vertical Box give `body` its full remaining height
        // rather than just `paned`'s minimum — a GTK widget's computed
        // expand propagates up through its ancestors, so without it here
        // nothing in this card's tree ever requests more than its natural
        // size and a resized note just leaves blank space below its content
        // instead of actually filling the card (see `PlaceholderNode::new`,
        // which already relies on the same propagation via `content`).
        let edit_scroller = gtk4::ScrolledWindow::new();
        edit_scroller.set_child(Some(&edit_view));
        edit_scroller.set_hexpand(true);
        edit_scroller.set_vexpand(true);
        let preview_scroller = gtk4::ScrolledWindow::new();
        preview_scroller.set_child(Some(&preview_view));
        preview_scroller.set_hexpand(true);
        preview_scroller.set_vexpand(true);

        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_start_child(Some(&edit_scroller));
        paned.set_end_child(Some(&preview_scroller));
        paned.set_resize_start_child(true);
        paned.set_resize_end_child(true);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_size_request(220, 160);

        let banner_label = gtk4::Label::new(None);
        banner_label.set_wrap(true);
        banner_label.set_xalign(0.0);
        banner_label.set_hexpand(true);
        let banner_primary = gtk4::Button::new();
        let banner_secondary = gtk4::Button::new();
        let banner = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        banner.add_css_class("file-banner");
        banner.append(&banner_label);
        banner.append(&banner_secondary);
        banner.append(&banner_primary);
        banner.set_visible(false);
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content.append(&banner);
        content.append(&paned);

        let resize_handle = resize_handle();
        let body = gtk4::Overlay::new();
        body.set_child(Some(&content));
        body.add_overlay(&resize_handle);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&title_bar);
        container.append(&body);
        container.set_css_classes(&["card", &format!("note-{color}")]);

        let collapse = wire_minimize(
            &minimize_button,
            &body,
            &container,
            collapsed,
            on_collapse_toggle,
        );

        let node = NoteNode {
            container,
            edit_view,
            preview_view,
            preview_refresh_scheduled: Rc::new(Cell::new(false)),
            edit_scroller,
            preview_scroller,
            file_popover,
            title_label,
            file_label,
            sync_label,
            banner,
            banner_label,
            banner_primary,
            banner_secondary,
            drag_handle,
            close_button,
            resize_handle,
            collapse,
        };
        node.set_view_mode(initial_mode);
        node
    }

    pub fn set_title(&self, title: &str) {
        self.title_label.set_text(title);
    }

    /// Shows (or, with `None`, hides) the backing file's path in the title
    /// bar.
    pub fn set_file_backing(&self, path: Option<&str>) {
        match path {
            Some(path) => {
                self.file_label.set_text(path);
                self.file_label
                    .set_tooltip_text(Some(&format!("Synced with project file {path}")));
                self.file_label.set_visible(true);
                self.sync_label.set_visible(true);
            }
            None => {
                self.file_label.set_visible(false);
                self.sync_label.set_visible(false);
                self.hide_banner();
            }
        }
    }

    pub fn set_sync_status(&self, status: &str) {
        self.sync_label.set_text(status);
    }

    pub fn show_banner(&self, message: &str, primary: &str, secondary: &str) {
        self.banner_label.set_text(message);
        self.banner_primary.set_label(primary);
        self.banner_secondary.set_label(secondary);
        self.banner.set_visible(true);
    }

    pub fn hide_banner(&self) {
        self.banner.set_visible(false);
    }

    pub fn banner_visible(&self) -> bool {
        self.banner.is_visible()
    }

    /// Calls `f` on a double-click in the rendered Preview — the quick way
    /// into editing a note, now that the mode switch lives in the menu.
    pub fn connect_preview_double_click(&self, f: impl Fn() + 'static) {
        let click = gtk4::GestureClick::new();
        click.connect_pressed(move |_gesture, n_press, _x, _y| {
            if n_press == 2 {
                f();
            }
        });
        self.preview_view.add_controller(click);
    }

    /// Schedules a re-render of the Preview pane from the current Edit
    /// source for once GTK has finished processing a resize or the card's
    /// initial placement on the canvas — a thematic break's width is
    /// computed from `preview_view`'s *current* allocation at render time
    /// (see `markdown::render_to_buffer`), which immediately after either of
    /// those is still the *previous* (or, for a brand-new card, nonexistent)
    /// allocation: GTK only recomputes it on the next main-loop pass, not
    /// synchronously inside `set_size_request`/`add_node`. A regular
    /// (default-priority) idle callback is guaranteed to run after GTK's own
    /// (higher-priority) resize/relayout processing, so by the time this
    /// fires, the new allocation is in. Edits don't need this: `edit_view`'s
    /// own `connect_changed` handler re-renders immediately, since typing
    /// never changes the view's width the way a resize does.
    ///
    /// Debounced via `preview_refresh_scheduled`: a live resize drag calls
    /// this once per pointer-motion tick, and queuing one idle callback per
    /// tick would mean dozens of redundant re-renders, each immediately
    /// superseded by the next, by the time the drag ends. At most one
    /// callback is ever in flight; since it reads `edit_view`/`preview_view`
    /// at the time it actually runs (not at schedule time), it always picks
    /// up the latest state regardless of how many ticks asked for it.
    pub fn refresh_preview_after_resize(&self) {
        if self.preview_refresh_scheduled.replace(true) {
            return;
        }
        let edit_view = self.edit_view.clone();
        let preview_view = self.preview_view.clone();
        let scheduled = self.preview_refresh_scheduled.clone();
        gtk4::glib::idle_add_local_once(move || {
            scheduled.set(false);
            let source = buffer_text(&edit_view.buffer());
            crate::markdown::render_to_buffer(&preview_view, &crate::markdown::parse(&source));
        });
    }

    pub fn set_view_mode(&self, mode: crate::model::NoteViewMode) {
        use crate::model::NoteViewMode;
        let (edit_visible, preview_visible) = match mode {
            NoteViewMode::Edit => (true, false),
            NoteViewMode::Preview => (false, true),
            NoteViewMode::Split => (true, true),
        };
        self.edit_scroller.set_visible(edit_visible);
        self.preview_scroller.set_visible(preview_visible);
    }
}

/// A plain-text annotation node: no Markdown rendering, just a label of text
/// — simpler than a `Note` for a quick caption. Same chrome shape as every
/// other card.
pub struct TextNode {
    pub container: gtk4::Box,
    pub text_view: gtk4::TextView,
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    pub resize_handle: gtk4::Box,
    pub collapse: CollapseHandle,
}

impl TextNode {
    pub fn new(
        initial_text: &str,
        collapsed: bool,
        on_collapse_toggle: impl Fn(bool) + 'static,
    ) -> TextNode {
        let (minimize_button, close_button) = title_buttons("Delete text node");

        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title_bar.add_css_class("note-title-bar");
        title_bar.append(&spacer());
        title_bar.append(&minimize_button);
        title_bar.append(&close_button);
        as_drag_handle(&title_bar);
        let drag_handle = title_bar.clone();

        let text_view = gtk4::TextView::new();
        text_view.buffer().set_text(initial_text);
        text_view.set_wrap_mode(gtk4::WrapMode::Word);
        text_view.set_size_request(220, 160);
        // Same fix as `NoteNode`'s scrollers: without an explicit expand
        // request somewhere in this card's tree, resizing it just leaves
        // blank space below the text instead of actually filling the card.
        text_view.set_vexpand(true);
        text_view.set_hexpand(true);
        text_view.set_top_margin(8);
        text_view.set_bottom_margin(8);
        text_view.set_left_margin(8);
        text_view.set_right_margin(8);
        text_view.add_css_class("note-text");

        let resize_handle = resize_handle();
        let body = gtk4::Overlay::new();
        body.set_child(Some(&text_view));
        body.add_overlay(&resize_handle);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&title_bar);
        container.append(&body);
        // Reuses the Note pastel-card palette (`.note-blue`/`.note-title-bar`
        // in style.css) rather than a separate, never-styled `.text-node`
        // class: a Text node used to render as near-black text on whatever
        // the system theme's default (often near-black) GtkTextView
        // background is — illegible, and nothing like the readable postit
        // look every other card on the canvas has. Blue (rather than Note's
        // default yellow) keeps a Text node visually distinguishable from a
        // Note at a glance.
        container.set_css_classes(&["card", "note-blue"]);

        let collapse = wire_minimize(
            &minimize_button,
            &body,
            &container,
            collapsed,
            on_collapse_toggle,
        );

        TextNode {
            container,
            text_view,
            drag_handle,
            close_button,
            resize_handle,
            collapse,
        }
    }
}

/// A stand-in widget for a node kind whose real behavior doesn't exist yet
/// (`FileTree`, `Portal`, `Drawing`, `Group` — see `model.rs`'s doc comment).
/// Shows only a kind label and a short detail string so the node is visible,
/// selectable, movable and persistable on the canvas without pretending to
/// implement the feature it stands in for.
pub struct PlaceholderNode {
    pub container: gtk4::Box,
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    pub resize_handle: gtk4::Box,
    pub collapse: CollapseHandle,
}

impl PlaceholderNode {
    pub fn new(
        kind_label: &str,
        detail: &str,
        collapsed: bool,
        on_collapse_toggle: impl Fn(bool) + 'static,
    ) -> PlaceholderNode {
        let kind_title = gtk4::Label::new(Some(kind_label));
        kind_title.add_css_class("heading");

        let (minimize_button, close_button) = title_buttons("Remove node");

        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        title_bar.add_css_class("node-title-bar");
        title_bar.append(&kind_title);
        title_bar.append(&spacer());
        title_bar.append(&minimize_button);
        title_bar.append(&close_button);
        as_drag_handle(&title_bar);
        let drag_handle = title_bar.clone();

        let detail_label = gtk4::Label::new(Some(detail));
        detail_label.add_css_class("dim-label");
        detail_label.set_wrap(true);
        let placeholder_icon = gtk4::Image::from_icon_name("content-loading-symbolic");
        placeholder_icon.set_pixel_size(32);
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        content.set_valign(gtk4::Align::Center);
        content.set_halign(gtk4::Align::Center);
        content.set_vexpand(true);
        content.set_hexpand(true);
        content.append(&placeholder_icon);
        content.append(&detail_label);
        content.set_size_request(220, 160);

        let resize_handle = resize_handle();
        let body = gtk4::Overlay::new();
        body.set_child(Some(&content));
        body.add_overlay(&resize_handle);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&title_bar);
        container.append(&body);
        container.set_css_classes(&["card", "placeholder-node"]);

        let collapse = wire_minimize(
            &minimize_button,
            &body,
            &container,
            collapsed,
            on_collapse_toggle,
        );

        PlaceholderNode {
            container,
            drag_handle,
            close_button,
            resize_handle,
            collapse,
        }
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
        let node = super::SessionNode::new("probe", false, |_| {});
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
