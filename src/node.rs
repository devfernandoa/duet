//! A GTK widget that displays a terminal. Has zero PTY/process-management
//! knowledge (that's `session.rs`'s job) and zero knowledge of canvas
//! positioning (that's `canvas.rs`'s job) — it only knows how to display
//! bytes (`feed`) and report what the user typed (`connect_commit`).

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

        SessionNode {
            container,
            title_bar,
            terminal,
        }
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
/// no PTY/process knowledge (unlike `SessionNode`).
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
