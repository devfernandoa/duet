//! GTK widgets for Milestone 6's project-file nodes: [`FileTreeNode`] (a
//! browsable tree / search-results list) and [`EditorNode`] (a GtkSourceView
//! 5 editor, read-only diff viewer, or image viewer). Like `node.rs`, these
//! are presentation objects only: they draw the plain data `app` hands them
//! (`FileTreeItem`s, text, bytes) and report user intent through callbacks.
//! They never touch the filesystem, Git, `NodeRecord`s or `App` — every file
//! operation happens in `project::*`, reached through `app::files`.
//!
//! Self-contained widget behavior that needs no domain knowledge — find /
//! replace, go to line, syntax highlighting, line selection — lives here.

use crate::node::{CollapseHandle, as_drag_handle, resize_handle, wire_minimize};
use gtk4::prelude::*;
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

type ItemCallback = Rc<RefCell<Option<Rc<dyn Fn(FileTreeItem)>>>>;
type ContextCallback = Rc<RefCell<Option<Rc<dyn Fn(FileTreeItem, gtk4::Widget)>>>>;
type ActionCallback = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// The prefix a dragged FileTree row's string payload carries, so the canvas
/// drop target can tell a project file apart from arbitrary dropped text.
pub const FILE_DRAG_PREFIX: &str = "duet-file:";

/// One row of a FileTree node, as plain data.
#[derive(Debug, Clone, PartialEq)]
pub enum FileTreeItem {
    Entry {
        path: String,
        name: String,
        depth: usize,
        is_dir: bool,
        expanded: bool,
        marker: Option<char>,
        /// A symlink that can't be followed (it points outside the project).
        unreachable_link: bool,
    },
    /// A fuzzy filename search hit; `positions` are matched char indices.
    NameHit {
        path: String,
        positions: Vec<usize>,
    },
    /// A content search hit.
    ContentHit {
        path: String,
        line: u64,
        text: String,
    },
    Message(String),
}

impl FileTreeItem {
    pub fn path(&self) -> Option<&str> {
        match self {
            FileTreeItem::Entry { path, .. }
            | FileTreeItem::NameHit { path, .. }
            | FileTreeItem::ContentHit { path, .. } => Some(path),
            FileTreeItem::Message(_) => None,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, FileTreeItem::Entry { is_dir: true, .. })
    }
}

fn flat_button(icon: &str, tooltip: &str) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon);
    button.add_css_class("flat");
    button.set_tooltip_text(Some(tooltip));
    button
}

/// The shared card chrome (title bar, minimize and close, resize grip,
/// collapsible body) around `content`. Callers append their own title
/// widgets to `title_bar`; `finish_chrome` then appends the minimize/close
/// pair and makes the whole bar the drag handle, the same as every other
/// card (see `node::as_drag_handle`).
struct Chrome {
    container: gtk4::Box,
    title_bar: gtk4::Box,
    minimize_button: gtk4::Button,
    close_button: gtk4::Button,
    resize_handle: gtk4::Box,
    body: gtk4::Overlay,
}

fn chrome(content: &impl IsA<gtk4::Widget>, close_tooltip: &str) -> Chrome {
    let minimize_button = gtk4::Button::from_icon_name("go-up-symbolic");
    minimize_button.add_css_class("flat");
    let close_button = flat_button("window-close-symbolic", close_tooltip);
    let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    title_bar.add_css_class("node-title-bar");
    let resize_handle = resize_handle();
    let body = gtk4::Overlay::new();
    body.set_child(Some(content));
    body.add_overlay(&resize_handle);
    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    container.append(&title_bar);
    container.append(&body);
    Chrome {
        container,
        title_bar,
        minimize_button,
        close_button,
        resize_handle,
        body,
    }
}

fn finish_chrome(
    chrome: &Chrome,
    collapsed: bool,
    on_collapse_toggle: impl Fn(bool) + 'static,
) -> CollapseHandle {
    chrome.title_bar.append(&chrome.minimize_button);
    chrome.title_bar.append(&chrome.close_button);
    as_drag_handle(&chrome.title_bar);
    wire_minimize(
        &chrome.minimize_button,
        &chrome.body,
        &chrome.container,
        collapsed,
        on_collapse_toggle,
    )
}

/// An empty, expanding title-bar spacer.
fn spacer() -> gtk4::Box {
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    spacer
}

/// A FileTree card: a search field over a `ListBox` of rows. Navigation,
/// filters, refresh and commit live in the card's right-click menu (built
/// by `app::files`), not on a toolbar. Rows are rebuilt wholesale by
/// `set_items` — the tree only ever contains the directories the user
/// expanded, so this stays small, and it keeps the widget a pure function of
/// the data `app::files` computed.
#[derive(Clone)]
pub struct FileTreeNode {
    pub container: gtk4::Box,
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    pub resize_handle: gtk4::Box,
    pub collapse: CollapseHandle,
    pub title_label: gtk4::Label,
    pub search_entry: gtk4::SearchEntry,
    pub status_label: gtk4::Label,
    pub list: gtk4::ListBox,
    /// The right-click menu, parented to `list` once and reused (pointed at
    /// the clicked row) rather than created and unparented per click:
    /// unparenting a just-closed popover crashes GTK 4.14 inside
    /// `gdk_surface_request_motion`.
    pub menu_popover: gtk4::Popover,
    /// A second reusable popover (the "ask an agent" prompt), parented to
    /// the title.
    pub aux_popover: gtk4::Popover,
    items: Rc<RefCell<Vec<FileTreeItem>>>,
    /// Set while `set_items` rebuilds the list, so the selection churn that
    /// rebuilding causes isn't reported as user selection.
    suppress: Rc<Cell<bool>>,
    on_toggle: ItemCallback,
    on_select: ItemCallback,
}

impl FileTreeNode {
    pub fn new(collapsed: bool, on_collapse_toggle: impl Fn(bool) + 'static) -> FileTreeNode {
        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::Single);
        list.set_activate_on_single_click(false);
        list.add_css_class("file-tree-list");
        list.add_css_class(crate::canvas::NO_CANVAS_PAN_CLASS);
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_child(Some(&list));
        scroller.set_hexpand(true);
        scroller.set_vexpand(true);
        scroller.set_hscrollbar_policy(gtk4::PolicyType::Never);

        let search_entry = gtk4::SearchEntry::new();
        search_entry.set_placeholder_text(Some("Find file… (> to search contents)"));
        search_entry.set_margin_start(4);
        search_entry.set_margin_end(4);

        let status_label = gtk4::Label::new(None);
        status_label.add_css_class("dim-label");
        status_label.add_css_class("caption");
        status_label.set_xalign(0.0);
        status_label.set_margin_start(6);
        status_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);

        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        search_entry.set_margin_top(4);
        content.append(&search_entry);
        content.append(&scroller);
        content.append(&status_label);
        content.set_size_request(260, 220);

        let chrome = chrome(&content, "Remove file tree");
        let icon = gtk4::Image::from_icon_name("folder-symbolic");
        let title_label = gtk4::Label::new(Some("Files"));
        title_label.add_css_class("heading");
        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        title_label.set_max_width_chars(18);
        chrome.title_bar.append(&icon);
        chrome.title_bar.append(&title_label);
        chrome.title_bar.append(&spacer());
        let collapse = finish_chrome(&chrome, collapsed, on_collapse_toggle);
        chrome
            .container
            .set_css_classes(&["card", "file-tree-node"]);
        let menu_popover = gtk4::Popover::new();
        menu_popover.set_has_arrow(false);
        menu_popover.set_parent(&list);
        let aux_popover = gtk4::Popover::new();
        aux_popover.set_parent(&title_label);

        let node = FileTreeNode {
            drag_handle: chrome.title_bar.clone(),
            container: chrome.container,
            close_button: chrome.close_button,
            resize_handle: chrome.resize_handle,
            collapse,
            title_label,
            search_entry,
            status_label,
            list,
            menu_popover,
            aux_popover,
            items: Rc::new(RefCell::new(Vec::new())),
            suppress: Rc::new(Cell::new(false)),
            on_toggle: Rc::new(RefCell::new(None)),
            on_select: Rc::new(RefCell::new(None)),
        };
        node.list.connect_row_selected({
            let items = node.items.clone();
            let suppress = node.suppress.clone();
            let on_select = node.on_select.clone();
            move |_list, row| {
                if suppress.get() {
                    return;
                }
                let Some(row) = row else { return };
                let item = items.borrow().get(row.index() as usize).cloned();
                let callback = on_select.borrow().clone();
                if let (Some(item), Some(callback)) = (item, callback) {
                    callback(item);
                }
            }
        });
        node
    }

    pub fn items(&self) -> Vec<FileTreeItem> {
        self.items.borrow().clone()
    }

    /// Replaces every row. `selected` (a path) is re-selected if present.
    pub fn set_items(&self, items: Vec<FileTreeItem>, selected: Option<&str>) {
        self.suppress.set(true);
        // Rows only: `menu_popover` is also a child of `list`.
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let mut selected_row = None;
        for item in &items {
            let row = self.build_row(item);
            if selected.is_some() && item.path() == selected && selected_row.is_none() {
                selected_row = Some(row.clone());
            }
            self.list.append(&row);
        }
        *self.items.borrow_mut() = items;
        if let Some(row) = selected_row {
            self.list.select_row(Some(&row));
        }
        self.suppress.set(false);
    }

    fn build_row(&self, item: &FileTreeItem) -> gtk4::ListBoxRow {
        let row = gtk4::ListBoxRow::new();
        let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        line.set_margin_top(1);
        line.set_margin_bottom(1);
        match item {
            FileTreeItem::Entry {
                name,
                depth,
                is_dir,
                expanded,
                marker,
                unreachable_link,
                ..
            } => {
                line.set_margin_start(4 + (*depth as i32) * 14);
                let arrow = gtk4::Label::new(Some(if !is_dir {
                    " "
                } else if *expanded {
                    "▾"
                } else {
                    "▸"
                }));
                arrow.set_width_chars(1);
                if *is_dir {
                    arrow.set_cursor_from_name(Some("pointer"));
                    let click = gtk4::GestureClick::new();
                    click.connect_pressed({
                        let on_toggle = self.on_toggle.clone();
                        let item = item.clone();
                        move |gesture, _, _, _| {
                            gesture.set_state(gtk4::EventSequenceState::Claimed);
                            let callback = on_toggle.borrow().clone();
                            if let Some(callback) = callback {
                                callback(item.clone());
                            }
                        }
                    });
                    arrow.add_controller(click);
                }
                let icon = gtk4::Image::from_icon_name(if *unreachable_link {
                    "emblem-symbolic-link"
                } else if *is_dir {
                    "folder-symbolic"
                } else {
                    "text-x-generic-symbolic"
                });
                let label = gtk4::Label::new(Some(name));
                label.set_xalign(0.0);
                label.set_hexpand(true);
                label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
                line.append(&arrow);
                line.append(&icon);
                line.append(&label);
                if let Some(marker) = marker {
                    let badge = gtk4::Label::new(Some(&marker.to_string()));
                    badge.add_css_class("git-marker");
                    badge.add_css_class(git_marker_class(*marker));
                    badge.set_margin_end(4);
                    line.append(&badge);
                }
            }
            FileTreeItem::NameHit { path, positions } => {
                line.set_margin_start(6);
                let label = gtk4::Label::new(None);
                label.set_markup(&highlight_markup(path, positions));
                label.set_xalign(0.0);
                label.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
                line.append(&gtk4::Image::from_icon_name("text-x-generic-symbolic"));
                line.append(&label);
            }
            FileTreeItem::ContentHit {
                path,
                line: number,
                text,
            } => {
                line.set_orientation(gtk4::Orientation::Vertical);
                line.set_margin_start(6);
                let location = gtk4::Label::new(Some(&format!("{path}:{number}")));
                location.add_css_class("dim-label");
                location.add_css_class("caption");
                location.set_xalign(0.0);
                location.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
                let snippet = gtk4::Label::new(Some(text.trim()));
                snippet.add_css_class("monospace");
                snippet.set_xalign(0.0);
                snippet.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                line.append(&location);
                line.append(&snippet);
            }
            FileTreeItem::Message(message) => {
                let label = gtk4::Label::new(Some(message));
                label.add_css_class("dim-label");
                label.set_wrap(true);
                label.set_xalign(0.0);
                label.set_margin_start(6);
                line.append(&label);
                row.set_selectable(false);
                row.set_activatable(false);
            }
        }
        if let Some(path) = item.path() {
            row.set_tooltip_text(Some(path));
            // Drag a row onto the canvas to open it there.
            let drag = gtk4::DragSource::new();
            drag.set_actions(gtk4::gdk::DragAction::COPY);
            let payload = format!("{FILE_DRAG_PREFIX}{path}");
            drag.connect_prepare(move |_source, _x, _y| {
                Some(gtk4::gdk::ContentProvider::for_value(&payload.to_value()))
            });
            row.add_controller(drag);
        }
        row.set_child(Some(&line));
        row
    }

    /// Double-click or Enter on a row.
    pub fn connect_activate(&self, f: impl Fn(FileTreeItem) + 'static) {
        let items = self.items.clone();
        self.list.connect_row_activated(move |_list, row| {
            let item = items.borrow().get(row.index() as usize).cloned();
            if let Some(item) = item {
                f(item);
            }
        });
    }

    /// A click on a directory row's expander arrow.
    pub fn connect_toggle(&self, f: impl Fn(FileTreeItem) + 'static) {
        *self.on_toggle.borrow_mut() = Some(Rc::new(f));
    }

    /// The user selected a row (not reported while `set_items` rebuilds).
    pub fn connect_select(&self, f: impl Fn(FileTreeItem) + 'static) {
        *self.on_select.borrow_mut() = Some(Rc::new(f));
    }

    /// Points `menu_popover` at `row` (a row of `list`).
    pub fn point_menu_at(&self, row: &gtk4::Widget) {
        if let Some(bounds) = row.compute_bounds(&self.list) {
            self.menu_popover
                .set_pointing_to(Some(&gtk4::gdk::Rectangle::new(
                    bounds.x() as i32 + 24,
                    bounds.y() as i32,
                    1,
                    bounds.height() as i32,
                )));
        }
    }

    /// A right-click on a row: `f` gets the row's item and the row widget
    /// (see `point_menu_at`).
    pub fn connect_context_menu(&self, f: impl Fn(FileTreeItem, gtk4::Widget) + 'static) {
        let callback: ContextCallback = Rc::new(RefCell::new(Some(Rc::new(f))));
        let click = gtk4::GestureClick::new();
        click.set_button(gtk4::gdk::BUTTON_SECONDARY);
        let items = self.items.clone();
        let list = self.list.clone();
        click.connect_pressed(move |gesture, _n, _x, y| {
            let Some(row) = list.row_at_y(y as i32) else {
                return;
            };
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let item = items.borrow().get(row.index() as usize).cloned();
            let callback = callback.borrow().clone();
            if let (Some(item), Some(callback)) = (item, callback) {
                list.select_row(Some(&row));
                callback(item, row.upcast());
            }
        });
        self.list.add_controller(click);
    }
}

fn git_marker_class(marker: char) -> &'static str {
    match marker {
        'M' => "git-modified",
        'A' => "git-added",
        'D' => "git-deleted",
        'R' | 'C' => "git-renamed",
        '?' => "git-untracked",
        'U' => "git-conflict",
        _ => "git-changed",
    }
}

/// Pango markup for `text` with the chars at `positions` in bold.
pub fn highlight_markup(text: &str, positions: &[usize]) -> String {
    let mut markup = String::new();
    for (index, c) in text.chars().enumerate() {
        let escaped = gtk4::glib::markup_escape_text(&c.to_string());
        if positions.contains(&index) {
            markup.push_str(&format!("<b>{escaped}</b>"));
        } else {
            markup.push_str(&escaped);
        }
    }
    markup
}

/// What an `EditorNode` is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorDisplay {
    Source,
    Image,
    Message,
}

/// An editor card: a GtkSourceView 5 view with line numbers, syntax
/// highlighting, auto-indent, find/replace and go-to-line, plus a banner
/// for external-change/conflict prompts. Also shows images and an
/// explanatory message for files it can't display.
#[derive(Clone)]
pub struct EditorNode {
    pub container: gtk4::Box,
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    pub resize_handle: gtk4::Box,
    pub collapse: CollapseHandle,
    pub title_label: gtk4::Label,
    pub status_label: gtk4::Label,
    pub view: sourceview5::View,
    pub buffer: sourceview5::Buffer,
    search_bar: gtk4::Box,
    replace_row: gtk4::Box,
    search_entry: gtk4::SearchEntry,
    replace_entry: gtk4::Entry,
    match_label: gtk4::Label,
    search_context: sourceview5::SearchContext,
    goto_popover: gtk4::Popover,
    goto_entry: gtk4::Entry,
    pub banner: gtk4::Box,
    banner_label: gtk4::Label,
    pub banner_primary: gtk4::Button,
    pub banner_secondary: gtk4::Button,
    stack: gtk4::Stack,
    picture: gtk4::Picture,
    message_label: gtk4::Label,
    on_save: ActionCallback,
    read_only: Rc<Cell<bool>>,
    /// A reusable popover (the "ask an agent" prompt) parented to the title
    /// — see `FileTreeNode::menu_popover` for why it's never unparented.
    pub aux_popover: gtk4::Popover,
}

impl EditorNode {
    pub fn new(
        title: &str,
        collapsed: bool,
        on_collapse_toggle: impl Fn(bool) + 'static,
    ) -> EditorNode {
        let buffer = sourceview5::Buffer::new(None);
        buffer.set_highlight_syntax(true);
        buffer.set_highlight_matching_brackets(true);
        let view = sourceview5::View::with_buffer(&buffer);
        view.set_show_line_numbers(true);
        view.set_auto_indent(true);
        view.set_indent_on_tab(true);
        view.set_tab_width(4);
        view.set_indent_width(4);
        view.set_insert_spaces_instead_of_tabs(true);
        view.set_highlight_current_line(true);
        view.set_smart_backspace(true);
        view.set_monospace(true);
        view.set_left_margin(4);
        let scheme_name = if libadwaita::StyleManager::default().is_dark() {
            "Adwaita-dark"
        } else {
            "Adwaita"
        };
        if let Some(scheme) = sourceview5::StyleSchemeManager::default().scheme(scheme_name) {
            buffer.set_style_scheme(Some(&scheme));
        }
        let source_scroller = gtk4::ScrolledWindow::new();
        source_scroller.set_child(Some(&view));
        source_scroller.set_hexpand(true);
        source_scroller.set_vexpand(true);

        let picture = gtk4::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk4::ContentFit::Contain);
        let message_label = gtk4::Label::new(None);
        message_label.set_wrap(true);
        message_label.add_css_class("dim-label");
        let stack = gtk4::Stack::new();
        stack.add_named(&source_scroller, Some("source"));
        stack.add_named(&picture, Some("image"));
        stack.add_named(&message_label, Some("message"));
        stack.set_hexpand(true);
        stack.set_vexpand(true);

        // Find / replace bar.
        let search_settings = sourceview5::SearchSettings::new();
        search_settings.set_wrap_around(true);
        let search_context = sourceview5::SearchContext::new(&buffer, Some(&search_settings));
        search_context.set_highlight(true);
        let search_entry = gtk4::SearchEntry::new();
        search_entry.set_placeholder_text(Some("Find"));
        search_entry.set_hexpand(true);
        let prev_button = flat_button("go-up-symbolic", "Previous match (Shift+Enter)");
        let next_button = flat_button("go-down-symbolic", "Next match (Enter)");
        let case_toggle = gtk4::ToggleButton::with_label("Aa");
        case_toggle.add_css_class("flat");
        case_toggle.set_tooltip_text(Some("Match case"));
        let match_label = gtk4::Label::new(None);
        match_label.add_css_class("dim-label");
        let close_search = flat_button("window-close-symbolic", "Close (Esc)");
        let find_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        find_row.append(&search_entry);
        find_row.append(&match_label);
        find_row.append(&case_toggle);
        find_row.append(&prev_button);
        find_row.append(&next_button);
        find_row.append(&close_search);
        let replace_entry = gtk4::Entry::new();
        replace_entry.set_placeholder_text(Some("Replace with"));
        replace_entry.set_hexpand(true);
        let replace_button = gtk4::Button::with_label("Replace");
        let replace_all_button = gtk4::Button::with_label("All");
        replace_all_button.set_tooltip_text(Some("Replace all"));
        let replace_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        replace_row.append(&replace_entry);
        replace_row.append(&replace_button);
        replace_row.append(&replace_all_button);
        let search_bar = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        search_bar.add_css_class("editor-search-bar");
        search_bar.append(&find_row);
        search_bar.append(&replace_row);
        search_bar.set_visible(false);

        // External-change / conflict banner.
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
        content.append(&search_bar);
        content.append(&stack);
        content.set_size_request(320, 220);

        let chrome = chrome(&content, "Close editor");
        let title_label = gtk4::Label::new(Some(title));
        title_label.add_css_class("heading");
        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
        title_label.set_max_width_chars(24);
        let status_label = gtk4::Label::new(None);
        status_label.add_css_class("dim-label");
        status_label.add_css_class("caption");
        let goto_entry = gtk4::Entry::new();
        goto_entry.set_placeholder_text(Some("Line number"));
        goto_entry.set_input_purpose(gtk4::InputPurpose::Digits);
        let goto_popover = gtk4::Popover::new();
        goto_popover.set_child(Some(&goto_entry));
        goto_popover.set_parent(&title_label);

        let icon = gtk4::Image::from_icon_name("text-x-generic-symbolic");
        chrome.title_bar.append(&icon);
        chrome.title_bar.append(&title_label);
        chrome.title_bar.append(&spacer());
        chrome.title_bar.append(&status_label);
        let collapse = finish_chrome(&chrome, collapsed, on_collapse_toggle);
        chrome.container.set_css_classes(&["card", "editor-node"]);
        let aux_popover = gtk4::Popover::new();
        aux_popover.set_parent(&title_label);

        let node = EditorNode {
            drag_handle: chrome.title_bar.clone(),
            container: chrome.container,
            close_button: chrome.close_button,
            resize_handle: chrome.resize_handle,
            collapse,
            title_label,
            status_label,
            view,
            buffer,
            search_bar,
            replace_row,
            search_entry,
            replace_entry,
            match_label,
            search_context,
            goto_popover,
            goto_entry,
            banner,
            banner_label,
            banner_primary,
            banner_secondary,
            stack,
            picture,
            message_label,
            on_save: Rc::new(RefCell::new(None)),
            read_only: Rc::new(Cell::new(false)),
            aux_popover,
        };
        node.wire_search(
            &search_settings,
            &prev_button,
            &next_button,
            &case_toggle,
            &close_search,
            &replace_button,
            &replace_all_button,
        );
        node.wire_goto();
        node.wire_keys();
        node.buffer.connect_modified_changed({
            let title = node.title_label.clone();
            move |buffer| {
                let text = title.text().trim_start_matches("● ").to_string();
                if buffer.is_modified() {
                    title.set_text(&format!("● {text}"));
                } else {
                    title.set_text(&text);
                }
            }
        });
        node
    }

    #[allow(clippy::too_many_arguments)]
    fn wire_search(
        &self,
        settings: &sourceview5::SearchSettings,
        prev_button: &gtk4::Button,
        next_button: &gtk4::Button,
        case_toggle: &gtk4::ToggleButton,
        close_search: &gtk4::Button,
        replace_button: &gtk4::Button,
        replace_all_button: &gtk4::Button,
    ) {
        self.search_entry.connect_search_changed({
            let settings = settings.clone();
            let node = self.clone();
            move |entry| {
                let text = entry.text();
                settings.set_search_text((!text.is_empty()).then_some(text.as_str()));
                node.find(true, false);
            }
        });
        case_toggle.connect_toggled({
            let settings = settings.clone();
            move |toggle| settings.set_case_sensitive(toggle.is_active())
        });
        self.search_context.connect_occurrences_count_notify({
            let label = self.match_label.clone();
            move |context| {
                let count = context.occurrences_count();
                label.set_text(&match count {
                    n if n < 0 => String::new(),
                    0 => "no matches".to_string(),
                    1 => "1 match".to_string(),
                    n => format!("{n} matches"),
                });
            }
        });
        self.search_entry.connect_activate({
            let node = self.clone();
            move |_| node.find(true, true)
        });
        next_button.connect_clicked({
            let node = self.clone();
            move |_| node.find(true, true)
        });
        prev_button.connect_clicked({
            let node = self.clone();
            move |_| node.find(false, true)
        });
        close_search.connect_clicked({
            let node = self.clone();
            move |_| node.hide_search()
        });
        replace_button.connect_clicked({
            let node = self.clone();
            move |_| node.replace_current()
        });
        replace_all_button.connect_clicked({
            let node = self.clone();
            move |_| {
                node.replace_all();
            }
        });
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed({
            let node = self.clone();
            move |_controller, key, _code, modifiers| {
                if key == gtk4::gdk::Key::Escape {
                    node.hide_search();
                    return gtk4::glib::Propagation::Stop;
                }
                if key == gtk4::gdk::Key::Return
                    && modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK)
                {
                    node.find(false, true);
                    return gtk4::glib::Propagation::Stop;
                }
                gtk4::glib::Propagation::Proceed
            }
        });
        self.search_entry.add_controller(keys);
    }

    /// Moves the selection to the next (or previous) match. `skip_current`
    /// starts after the current selection instead of at it — false while
    /// typing, so the match under the cursor stays selected.
    fn find(&self, forward: bool, skip_current: bool) {
        let (start, end) = self.buffer.selection_bounds().unwrap_or_else(|| {
            let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
            (cursor, cursor)
        });
        let found = if forward {
            let from = if skip_current { end } else { start };
            self.search_context.forward(&from)
        } else {
            self.search_context.backward(&start)
        };
        if let Some((mut match_start, match_end, _wrapped)) = found {
            self.buffer.select_range(&match_start, &match_end);
            self.view
                .scroll_to_iter(&mut match_start, 0.1, false, 0.0, 0.0);
        }
    }

    fn replace_current(&self) {
        if self.read_only.get() {
            return;
        }
        let replacement = self.replace_entry.text();
        if let Some((mut start, mut end)) = self.buffer.selection_bounds() {
            // Only replace if the selection *is* a match; otherwise just
            // move to the next one first.
            if self
                .search_context
                .replace(&mut start, &mut end, &replacement)
                .is_err()
            {
                self.find(true, false);
                return;
            }
        }
        self.find(true, false);
    }

    /// Replaces every match with the replace field's text, as one undo
    /// step, and returns how many were replaced. Deliberately not
    /// `SearchContext::replace_all`: its binding asserts the C function's
    /// replacement *count* is a success boolean, so "Replace all" with no
    /// matches would panic.
    pub fn replace_all(&self) -> u32 {
        if self.read_only.get() {
            return 0;
        }
        let replacement = self.replace_entry.text();
        let mut count = 0;
        let mut from = self.buffer.start_iter();
        self.buffer.begin_user_action();
        while let Some((mut start, mut end, wrapped)) = self.search_context.forward(&from) {
            if wrapped && count > 0 {
                break;
            }
            if self
                .search_context
                .replace(&mut start, &mut end, &replacement)
                .is_err()
            {
                break;
            }
            count += 1;
            from = end;
        }
        self.buffer.end_user_action();
        count
    }

    pub fn show_search(&self, with_replace: bool) {
        self.search_bar.set_visible(true);
        self.replace_row
            .set_visible(with_replace && !self.read_only.get());
        if let Some((start, end)) = self.buffer.selection_bounds() {
            let selected = self.buffer.text(&start, &end, false);
            if !selected.contains('\n') && !selected.is_empty() {
                self.search_entry.set_text(&selected);
            }
        }
        self.search_entry.grab_focus();
    }

    fn hide_search(&self) {
        self.search_bar.set_visible(false);
        self.view.grab_focus();
    }

    fn wire_goto(&self) {
        self.goto_entry.connect_activate({
            let node = self.clone();
            move |entry| {
                if let Ok(line) = entry.text().trim().parse::<u32>() {
                    node.go_to_line(line);
                }
                node.goto_popover.popdown();
                node.view.grab_focus();
            }
        });
    }

    pub fn show_goto(&self) {
        self.goto_entry.set_text("");
        self.goto_popover.popup();
        self.goto_entry.grab_focus();
    }

    /// Moves the cursor to the start of 1-based `line` (clamped) and scrolls
    /// it into view.
    pub fn go_to_line(&self, line: u32) {
        let last = self.buffer.line_count().max(1);
        let target = (line.max(1) as i32).min(last) - 1;
        if let Some(mut iter) = self.buffer.iter_at_line(target) {
            self.buffer.place_cursor(&iter);
            self.view.scroll_to_iter(&mut iter, 0.2, true, 0.0, 0.3);
        }
    }

    fn wire_keys(&self) {
        // Local to this editor's own view, not application accelerators:
        // a focused terminal elsewhere on the canvas keeps every key.
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed({
            let node = self.clone();
            move |_controller, key, _code, modifiers| {
                if !modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                    return gtk4::glib::Propagation::Proceed;
                }
                match key.to_lower() {
                    gtk4::gdk::Key::s => node.save(),
                    gtk4::gdk::Key::f => node.show_search(false),
                    gtk4::gdk::Key::h => node.show_search(true),
                    gtk4::gdk::Key::g => node.show_goto(),
                    _ => return gtk4::glib::Propagation::Proceed,
                }
                gtk4::glib::Propagation::Stop
            }
        });
        self.view.add_controller(keys);
    }

    /// Ctrl+S or the menu's Save.
    pub fn connect_save(&self, f: impl Fn() + 'static) {
        *self.on_save.borrow_mut() = Some(Rc::new(f));
    }

    /// Runs the `connect_save` callback.
    pub fn save(&self) {
        let callback = self.on_save.borrow().clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only.get()
    }

    /// Replaces the buffer's content without making the replacement itself
    /// undoable, marks it unmodified, and keeps the cursor near where it was.
    pub fn set_text(&self, text: &str) {
        let cursor_line = self.buffer.iter_at_mark(&self.buffer.get_insert()).line();
        self.buffer.begin_irreversible_action();
        self.buffer.set_text(text);
        self.buffer.end_irreversible_action();
        self.buffer.set_modified(false);
        if let Some(iter) = self.buffer.iter_at_line(cursor_line) {
            self.buffer.place_cursor(&iter);
        }
        self.stack.set_visible_child_name("source");
    }

    pub fn text(&self) -> String {
        self.buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), true)
            .to_string()
    }

    pub fn is_modified(&self) -> bool {
        self.buffer.is_modified()
    }

    pub fn mark_saved(&self) {
        self.buffer.set_modified(false);
    }

    /// Picks highlighting from a file name (`auth.rs` -> Rust) or an explicit
    /// language id (`"diff"`), and indentation from the content: a file
    /// whose lines are tab-indented keeps tabs.
    pub fn configure_language(&self, file_name: Option<&str>, language_id: Option<&str>) {
        let manager = sourceview5::LanguageManager::default();
        let language = match language_id {
            Some(id) => manager.language(id),
            None => manager.guess_language(file_name, None::<&str>),
        };
        self.buffer.set_language(language.as_ref());
        let text = self.text();
        let tabs = text.lines().filter(|line| line.starts_with('\t')).count();
        let spaces = text.lines().filter(|line| line.starts_with("  ")).count();
        self.view.set_insert_spaces_instead_of_tabs(tabs <= spaces);
    }

    /// A read-only view (diffs): no typing, no save, no replace.
    pub fn set_read_only(&self, read_only: bool) {
        self.read_only.set(read_only);
        self.view.set_editable(!read_only);
    }

    pub fn display(&self) -> EditorDisplay {
        match self.stack.visible_child_name().as_deref() {
            Some("image") => EditorDisplay::Image,
            Some("message") => EditorDisplay::Message,
            _ => EditorDisplay::Source,
        }
    }

    /// Shows encoded image bytes (PNG, JPEG, SVG, ...). Errors if GTK can't
    /// decode them.
    pub fn show_image(&self, bytes: &[u8]) -> Result<(), String> {
        let texture = gtk4::gdk::Texture::from_bytes(&gtk4::glib::Bytes::from(bytes))
            .map_err(|error| error.to_string())?;
        self.picture.set_paintable(Some(&texture));
        self.stack.set_visible_child_name("image");
        self.set_read_only(true);
        Ok(())
    }

    /// Replaces the view with an explanatory message (a binary file, a
    /// deleted file, an error).
    pub fn show_message(&self, message: &str) {
        self.message_label.set_text(message);
        self.stack.set_visible_child_name("message");
    }

    pub fn set_status(&self, text: &str) {
        self.status_label.set_text(text);
    }

    pub fn show_banner(&self, message: &str, primary: &str, secondary: Option<&str>) {
        self.banner_label.set_text(message);
        self.banner_primary.set_label(primary);
        match secondary {
            Some(label) => {
                self.banner_secondary.set_label(label);
                self.banner_secondary.set_visible(true);
            }
            None => self.banner_secondary.set_visible(false),
        }
        self.banner.set_visible(true);
    }

    pub fn hide_banner(&self) {
        self.banner.set_visible(false);
    }

    /// The selected 1-based line range, or the cursor's line when nothing is
    /// selected. A selection ending at column 0 of a line doesn't count that
    /// line (selecting whole lines by dragging always ends there).
    pub fn selected_lines(&self) -> (u32, u32) {
        let (start, end) = self.buffer.selection_bounds().unwrap_or_else(|| {
            let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
            (cursor, cursor)
        });
        let first = start.line() as u32 + 1;
        let mut last = end.line() as u32 + 1;
        if last > first && end.line_offset() == 0 {
            last -= 1;
        }
        (first, last)
    }

    pub fn has_selection(&self) -> bool {
        self.buffer.has_selection()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_markup_bolds_positions_and_escapes() {
        assert_eq!(highlight_markup("a<b", &[0, 1]), "<b>a</b><b>&lt;</b>b");
    }

    #[test]
    fn file_tree_items_expose_paths() {
        let entry = FileTreeItem::Entry {
            path: "src".to_string(),
            name: "src".to_string(),
            depth: 0,
            is_dir: true,
            expanded: false,
            marker: None,
            unreachable_link: false,
        };
        assert_eq!(entry.path(), Some("src"));
        assert!(entry.is_dir());
        assert_eq!(FileTreeItem::Message("x".to_string()).path(), None);
    }

    /// Find/replace, go-to-line and line selection exercised against a real
    /// GtkSourceView buffer. Needs a display: `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a display"]
    fn editor_find_replace_goto_and_selection() {
        if gtk4::init().is_err() {
            return;
        }
        let editor = EditorNode::new("a.rs", false, |_| {});
        editor.set_text("fn one() {}\nfn two() {}\nfn three() {}\n");
        assert!(!editor.is_modified());
        editor.configure_language(Some("a.rs"), None);
        assert_eq!(
            editor
                .buffer
                .language()
                .map(|l| l.id().to_string())
                .as_deref(),
            Some("rust")
        );

        editor.go_to_line(2);
        assert_eq!(editor.selected_lines(), (2, 2));

        editor.show_search(true);
        editor.replace_entry.set_text("pub fn");
        // No search text yet: nothing to replace, and no panic.
        assert_eq!(editor.replace_all(), 0);
        editor.search_entry.set_text("fn");
        // `search-changed` is emitted after GTK's own typing delay.
        let context = gtk4::glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while editor.match_label.text() != "3 matches" && std::time::Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(editor.match_label.text(), "3 matches");
        assert_eq!(editor.replace_all(), 3);
        assert_eq!(
            editor.text(),
            "pub fn one() {}\npub fn two() {}\npub fn three() {}\n"
        );
        assert!(editor.is_modified());
        // One undo step restores everything.
        editor.buffer.undo();
        assert_eq!(editor.text(), "fn one() {}\nfn two() {}\nfn three() {}\n");

        let start = editor.buffer.iter_at_line(0).unwrap();
        let end = editor.buffer.iter_at_line(2).unwrap();
        editor.buffer.select_range(&start, &end);
        assert_eq!(editor.selected_lines(), (1, 2));

        editor.set_read_only(true);
        assert!(!editor.view.is_editable());
    }
}
