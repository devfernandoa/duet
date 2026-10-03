//! The Portal card (Milestone 8): title bar, a browser toolbar (back,
//! forward, reload, URL field, open externally) and a slot the portal's
//! `WebView` is placed into. Like every other `node*.rs` widget it holds no
//! persisted state and no WebKit runtime of its own — the view is owned by
//! `portal_runtime::PortalRuntime` and only *shown* here (so it survives
//! this card being removed when its workspace is switched away), and every
//! button reports to `app::portals`, which calls the same `PortalService`
//! methods `duetctl portal ...` does.

use crate::node::{
    CollapseHandle, as_drag_handle, resize_handle, spacer, title_buttons, wire_minimize,
};
use gtk4::prelude::*;

#[derive(Clone)]
pub struct PortalNode {
    pub container: gtk4::Box,
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    pub resize_handle: gtk4::Box,
    pub collapse: CollapseHandle,
    pub title_label: gtk4::Label,
    pub title_entry: gtk4::Entry,
    pub status_label: gtk4::Label,
    pub back_button: gtk4::Button,
    pub forward_button: gtk4::Button,
    pub reload_button: gtk4::Button,
    pub url_entry: gtk4::Entry,
    pub external_button: gtk4::Button,
    /// Where the runtime's `WebView` is placed.
    web_slot: gtk4::Box,
    /// Shown over a portal that has no page yet.
    empty_hint: gtk4::Box,
}

fn tool_button(icon: &str, tooltip: &str) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon);
    button.add_css_class("flat");
    button.set_tooltip_text(Some(tooltip));
    button.set_focus_on_click(false);
    button
}

impl PortalNode {
    pub fn new(
        name: &str,
        url: &str,
        collapsed: bool,
        on_collapse_toggle: impl Fn(bool) + 'static,
    ) -> PortalNode {
        let icon = gtk4::Image::from_icon_name("web-browser-symbolic");
        let title_label = gtk4::Label::new(Some(name));
        title_label.add_css_class("heading");
        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title_label.set_tooltip_text(Some("Portal name — agents address it as @portal:<name>"));
        let title_entry = gtk4::Entry::new();
        title_entry.set_visible(false);
        title_entry.set_hexpand(true);
        let status_label = gtk4::Label::new(None);
        status_label.add_css_class("dim-label");
        status_label.add_css_class("caption");

        let (minimize_button, close_button) = title_buttons("Remove portal");
        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        title_bar.add_css_class("node-title-bar");
        title_bar.append(&icon);
        title_bar.append(&title_label);
        title_bar.append(&title_entry);
        title_bar.append(&status_label);
        title_bar.append(&spacer());
        title_bar.append(&minimize_button);
        title_bar.append(&close_button);
        as_drag_handle(&title_bar);

        let back_button = tool_button("go-previous-symbolic", "Back");
        let forward_button = tool_button("go-next-symbolic", "Forward");
        let reload_button = tool_button("view-refresh-symbolic", "Reload");
        let external_button = tool_button(
            "window-new-symbolic",
            "Open this page in your default browser",
        );
        back_button.set_sensitive(false);
        forward_button.set_sensitive(false);
        let url_entry = gtk4::Entry::new();
        url_entry.set_hexpand(true);
        url_entry.set_placeholder_text(Some("Enter a URL, e.g. localhost:3000"));
        url_entry.set_input_purpose(gtk4::InputPurpose::Url);
        url_entry.set_text(url);
        let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        toolbar.add_css_class("portal-toolbar");
        toolbar.append(&back_button);
        toolbar.append(&forward_button);
        toolbar.append(&reload_button);
        toolbar.append(&url_entry);
        toolbar.append(&external_button);

        let web_slot = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        web_slot.set_hexpand(true);
        web_slot.set_vexpand(true);
        web_slot.set_overflow(gtk4::Overflow::Hidden);

        let empty_hint = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        empty_hint.set_halign(gtk4::Align::Center);
        empty_hint.set_valign(gtk4::Align::Center);
        empty_hint.set_can_target(false);
        empty_hint.add_css_class("portal-empty-hint");
        let hint_icon = gtk4::Image::from_icon_name("web-browser-symbolic");
        hint_icon.set_pixel_size(32);
        let hint_text = gtk4::Label::new(Some(
            "Type a URL above — e.g. localhost:3000.\n\
             Connect an agent to this portal to let it browse and test here.",
        ));
        hint_text.set_justify(gtk4::Justification::Center);
        hint_text.set_wrap(true);
        empty_hint.append(&hint_icon);
        empty_hint.append(&hint_text);
        empty_hint.set_visible(url.trim().is_empty());
        let page = gtk4::Overlay::new();
        page.set_child(Some(&web_slot));
        page.add_overlay(&empty_hint);

        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content.append(&toolbar);
        content.append(&page);
        content.set_size_request(320, 240);

        let resize_handle = resize_handle();
        let body = gtk4::Overlay::new();
        body.set_child(Some(&content));
        body.add_overlay(&resize_handle);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&title_bar);
        container.append(&body);
        container.set_css_classes(&["card", "portal-node"]);

        let collapse = wire_minimize(
            &minimize_button,
            &body,
            &container,
            collapsed,
            on_collapse_toggle,
        );

        PortalNode {
            container,
            drag_handle: title_bar,
            close_button,
            resize_handle,
            collapse,
            title_label,
            title_entry,
            status_label,
            back_button,
            forward_button,
            reload_button,
            url_entry,
            external_button,
            web_slot,
            empty_hint,
        }
    }

    /// Shows `view` in this card, taking it from wherever it was shown
    /// before (a previous card for the same portal, from before a
    /// workspace switch).
    pub fn attach_view(&self, view: &impl IsA<gtk4::Widget>) {
        let view = view.as_ref();
        if let Some(parent) = view.parent() {
            if parent == *self.web_slot.upcast_ref::<gtk4::Widget>() {
                return;
            }
            match parent.downcast::<gtk4::Box>() {
                Ok(parent) => parent.remove(view),
                Err(_) => view.unparent(),
            }
        }
        self.web_slot.append(view);
    }

    pub fn set_name(&self, name: &str) {
        self.title_label.set_text(name);
    }

    /// Mirrors the page's URL into the field — unless the user is typing in
    /// it, which a background redirect must never clobber.
    pub fn show_url(&self, url: &str) {
        if !self.url_entry.has_focus() && self.url_entry.text() != url {
            self.url_entry.set_text(url);
        }
        let blank = url.trim().is_empty() || url == "about:blank";
        if self.empty_hint.is_visible() != blank {
            self.empty_hint.set_visible(blank);
        }
    }

    pub fn show_navigation_state(&self, can_go_back: bool, can_go_forward: bool, loading: bool) {
        self.back_button.set_sensitive(can_go_back);
        self.forward_button.set_sensitive(can_go_forward);
        self.status_label
            .set_text(if loading { "loading…" } else { "" });
    }

    pub fn show_page_title(&self, title: &str) {
        self.title_label
            .set_tooltip_text(Some(&if title.is_empty() {
                "Portal name — agents address it as @portal:<name>".to_string()
            } else {
                format!("{title}\nAgents address this portal as @portal:<name>")
            }));
    }
}
