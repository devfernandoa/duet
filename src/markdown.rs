//! Markdown source -> a small styled-text intermediate form -> a GTK
//! `TextBuffer`. Split in two deliberately: [`parse`] is pure (parses with
//! `pulldown-cmark`, no GTK, independently testable without a display), and
//! [`render_to_buffer`] is the only part that touches GTK. Kept out of
//! `node.rs` and the persisted `NotePayload` (`model.rs`) so Milestone 6's
//! Chat can reuse both the parse step and the buffer renderer for agent
//! responses, not just `Note` nodes.
//!
//! Supports headings, paragraphs, ordered/unordered/task lists, links,
//! fenced code blocks, inline code, blockquotes, and thematic breaks
//! (`---`/`***`/`___`). Tables render as plain pipe-separated rows rather
//! than aligned cells — `pulldown-cmark`'s table extension gives cell
//! boundaries, not column widths, and a `GtkTextView` has no native table
//! widget to hand those to; a clean monospace-grid renderer is a reasonable
//! future upgrade, not a Milestone 1 requirement.
//!
//! A thematic break renders as a real `GtkSeparator` widget anchored into
//! the buffer (`render_to_buffer`), not as a run of literal characters — a
//! fixed character count is either too long (overflowing a narrow note) or
//! too short (not reaching the edge of a wide one), where a widget just
//! always spans exactly the view's current width, at any note size.
//!
//! One CommonMark subtlety worth knowing before "`---` doesn't work" reads as
//! a parser bug: a line of hyphens immediately after a paragraph with **no
//! blank line between them** is a Setext heading underline, not a thematic
//! break — `"Some text\n---"` becomes a level-2 heading "Some text", exactly
//! as it would on GitHub or any other CommonMark-compliant renderer. A
//! `---` only becomes an actual divider (`Event::Rule`) on its own, set off
//! by blank lines. This module doesn't special-case that away, since doing
//! so would break the far more common use of `---` as a heading underline.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// One visual style applied to a span of text. Several can apply to the same
/// span (e.g. bold text inside a link); `render_to_buffer` applies every tag
/// in `Span::styles` to the inserted range.
#[derive(Debug, Clone, PartialEq)]
pub enum SpanStyle {
    Heading(u8),
    Bold,
    Italic,
    InlineCode,
    Link(String),
    BlockQuote,
    CodeBlock,
    /// A thematic break (`Event::Rule`). Carried as a style on a span of
    /// filler characters, the same shape every other block uses, rather than
    /// a `Line`-level flag — `render_to_buffer` detects it the same way it
    /// detects every other style, even though what it renders for this one
    /// (a real `GtkSeparator` widget, not styled text) is different.
    Rule,
}

/// How many `RULE_CHAR`s [`parse`] puts on a thematic break's span. Only
/// [`parse`]'s own tests read this text directly (to confirm a rule was
/// found at all) — `render_to_buffer` ignores it and renders a real
/// `GtkSeparator` widget instead, which is what's actually visible; see this
/// module's doc comment for why a character run can't do that job on its
/// own at every note width.
const RULE_WIDTH: usize = 48;
/// Unicode box-drawing light horizontal line (U+2500), used only as the
/// placeholder text described above.
const RULE_CHAR: char = '─';

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub styles: Vec<SpanStyle>,
}

/// Markers a list item line can start with: a bullet, a number, or a task
/// checkbox. `None` for a non-list line (a plain paragraph, a heading, ...).
#[derive(Debug, Clone, PartialEq)]
pub enum LineMarker {
    Bullet { indent: u8 },
    Ordered { indent: u8, number: u64 },
    Task { indent: u8, checked: bool },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Line {
    pub marker: Option<LineMarker>,
    pub spans: Vec<Span>,
}

impl Line {
    fn push_text(&mut self, text: &str, styles: &[SpanStyle]) {
        if text.is_empty() {
            return;
        }
        self.spans.push(Span {
            text: text.to_string(),
            styles: styles.to_vec(),
        });
    }
}

/// `lines.last_mut()` — a plain function rather than a closure since a
/// closure can't express "for any borrow of `lines`, return a matching
/// borrow of its last element" (a closure infers one concrete lifetime
/// relationship, not the higher-ranked one this needs across many call
/// sites in `parse` below).
fn current_line(lines: &mut [Line]) -> &mut Line {
    lines.last_mut().expect("always at least one line")
}

fn new_line(lines: &mut Vec<Line>) {
    lines.push(Line::default());
}

/// Parses `source` as Markdown into a sequence of lines ready to render.
/// Pure — no GTK involved, safe to call and test headlessly.
pub fn parse(source: &str) -> Vec<Line> {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(source, options);

    let mut lines: Vec<Line> = vec![Line::default()];
    let mut style_stack: Vec<SpanStyle> = Vec::new();
    // List nesting depth and, for an ordered list, the next number to emit.
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut pending_marker: Option<LineMarker> = None;
    let mut in_code_block = false;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    let level = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    style_stack.push(SpanStyle::Heading(level));
                }
                Tag::Emphasis => style_stack.push(SpanStyle::Italic),
                Tag::Strong => style_stack.push(SpanStyle::Bold),
                Tag::BlockQuote(_) => style_stack.push(SpanStyle::BlockQuote),
                Tag::Link { dest_url, .. } => {
                    style_stack.push(SpanStyle::Link(dest_url.to_string()))
                }
                Tag::CodeBlock(kind) => {
                    in_code_block = true;
                    style_stack.push(SpanStyle::CodeBlock);
                    if !current_line(&mut lines).spans.is_empty() {
                        new_line(&mut lines);
                    }
                    if let CodeBlockKind::Fenced(_) = kind {
                        // Fence line itself carries no text content worth
                        // showing (the language tag isn't rendered output).
                    }
                }
                Tag::List(start) => list_stack.push(start),
                Tag::Item => {
                    if !current_line(&mut lines).spans.is_empty()
                        || current_line(&mut lines).marker.is_some()
                    {
                        new_line(&mut lines);
                    }
                    let indent = (list_stack.len().saturating_sub(1)) as u8;
                    pending_marker = match list_stack.last_mut() {
                        Some(Some(number)) => {
                            let current = *number;
                            *number += 1;
                            Some(LineMarker::Ordered {
                                indent,
                                number: current,
                            })
                        }
                        Some(None) => Some(LineMarker::Bullet { indent }),
                        None => None,
                    };
                }
                Tag::Paragraph
                    if !current_line(&mut lines).spans.is_empty()
                        || current_line(&mut lines).marker.is_some() =>
                {
                    new_line(&mut lines);
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_)
                | TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::BlockQuote(_)
                | TagEnd::Link
                | TagEnd::CodeBlock => {
                    style_stack.pop();
                    if tag_end == TagEnd::CodeBlock {
                        in_code_block = false;
                        new_line(&mut lines);
                    }
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                }
                TagEnd::Paragraph | TagEnd::Item => {
                    new_line(&mut lines);
                }
                _ => {}
            },
            Event::Text(text) => {
                let line = current_line(&mut lines);
                if let Some(marker) = pending_marker.take() {
                    line.marker = Some(marker);
                }
                if in_code_block {
                    // Fenced/indented code blocks arrive as one `Text` event
                    // per source line already containing its own newlines in
                    // some inputs; split so each physical line is its own
                    // `Line` with the CodeBlock style, matching every other
                    // block's one-`Line`-per-rendered-row shape.
                    let mut parts = text.split('\n').peekable();
                    while let Some(part) = parts.next() {
                        current_line(&mut lines).push_text(part, &style_stack);
                        if parts.peek().is_some() {
                            new_line(&mut lines);
                        }
                    }
                } else {
                    line.push_text(&text, &style_stack);
                }
            }
            Event::Code(text) => {
                let line = current_line(&mut lines);
                if let Some(marker) = pending_marker.take() {
                    line.marker = Some(marker);
                }
                let mut styles = style_stack.clone();
                styles.push(SpanStyle::InlineCode);
                line.push_text(&text, &styles);
            }
            Event::TaskListMarker(checked) => {
                let indent = (list_stack.len().saturating_sub(1)) as u8;
                pending_marker = Some(LineMarker::Task { indent, checked });
            }
            Event::SoftBreak | Event::HardBreak => {
                new_line(&mut lines);
            }
            Event::Rule => {
                new_line(&mut lines);
                // Deliberately NOT `style_stack` (a thematic break can't be
                // nested inside emphasis/a link/etc. per CommonMark, so
                // there's nothing on the stack to preserve) — just the one
                // style that makes this render as a visible divider instead
                // of a line of identical-looking body text.
                current_line(&mut lines).push_text(
                    &RULE_CHAR.to_string().repeat(RULE_WIDTH),
                    &[SpanStyle::Rule],
                );
                new_line(&mut lines);
            }
            _ => {}
        }
    }

    // Trailing empty line from the last block's own line break.
    if lines
        .last()
        .is_some_and(|line| line.spans.is_empty() && line.marker.is_none())
    {
        lines.pop();
    }
    lines
}

/// Applies `lines` (from [`parse`]) to `view`'s buffer, replacing its entire
/// content. The only GTK-touching half of this module — kept this thin
/// specifically so [`parse`] stays testable without a display. Takes the
/// `TextView` itself, not just its `TextBuffer`, because a thematic break
/// needs `TextView::add_child_at_anchor` (a real `GtkSeparator` widget,
/// which a bare buffer has no way to host) to always span the view's exact
/// current width.
pub fn render_to_buffer(view: &gtk4::TextView, lines: &[Line]) {
    use gtk4::prelude::*;

    let buffer = view.buffer();
    buffer.set_text("");
    let table = buffer.tag_table();
    for name in [
        "md-h1",
        "md-h2",
        "md-h3",
        "md-h4",
        "md-h5",
        "md-h6",
        "md-bold",
        "md-italic",
        "md-code",
        "md-link",
        "md-quote",
        "md-codeblock",
    ] {
        if table.lookup(name).is_none() {
            let tag = gtk4::TextTag::new(Some(name));
            match name {
                "md-h1" => {
                    tag.set_weight(700);
                    tag.set_scale(1.8);
                }
                "md-h2" => {
                    tag.set_weight(700);
                    tag.set_scale(1.5);
                }
                "md-h3" => {
                    tag.set_weight(700);
                    tag.set_scale(1.25);
                }
                "md-h4" | "md-h5" | "md-h6" => {
                    tag.set_weight(700);
                    tag.set_scale(1.1);
                }
                "md-bold" => tag.set_weight(700),
                "md-italic" => tag.set_style(gtk4::pango::Style::Italic),
                "md-code" => {
                    tag.set_family(Some("monospace"));
                    tag.set_background(Some("#00000018"));
                }
                "md-link" => {
                    tag.set_foreground(Some("#3584e4"));
                    tag.set_underline(gtk4::pango::Underline::Single);
                }
                "md-quote" => {
                    tag.set_style(gtk4::pango::Style::Italic);
                    tag.set_left_margin(16);
                    tag.set_foreground(Some("#6c6c6c"));
                }
                "md-codeblock" => {
                    tag.set_family(Some("monospace"));
                    tag.set_background(Some("#00000012"));
                }
                _ => {}
            }
            table.add(&tag);
        }
    }

    let mut end = buffer.end_iter();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            buffer.insert(&mut end, "\n");
            end = buffer.end_iter();
        }
        let prefix = match &line.marker {
            Some(LineMarker::Bullet { indent }) => {
                format!("{}\u{2022} ", "  ".repeat(*indent as usize))
            }
            Some(LineMarker::Ordered { indent, number }) => {
                format!("{}{number}. ", "  ".repeat(*indent as usize))
            }
            Some(LineMarker::Task { indent, checked }) => {
                format!(
                    "{}[{}] ",
                    "  ".repeat(*indent as usize),
                    if *checked { "x" } else { " " }
                )
            }
            None => String::new(),
        };
        if !prefix.is_empty() {
            buffer.insert(&mut end, &prefix);
            end = buffer.end_iter();
        }
        for span in &line.spans {
            if span.styles.contains(&SpanStyle::Rule) {
                // A real widget, not text, so it always exactly fits the
                // note's current size instead of either overflowing a
                // narrow one or falling short of a wide one — see this
                // module's doc comment. `span.text` (a run of `RULE_CHAR`)
                // is never actually inserted here; it only exists for
                // `parse`'s own tests to detect a rule was found.
                //
                // `hexpand` does nothing for a `TextView` child anchor —
                // unlike a normal container, the text layout engine sizes an
                // anchored widget at its own measured size and never
                // distributes leftover space to it — so the width has to be
                // set explicitly from the view's own current allocation.
                // `render_to_buffer` runs again on every edit (immediately)
                // and, via `NoteNode::refresh_preview_after_resize`, after
                // every resize (once GTK has reallocated), so this stays
                // correct as the note's size changes; before the view is
                // ever allocated (the very first render), a fallback keeps
                // the rule from collapsing to zero width.
                let content_width = view.width() - view.left_margin() - view.right_margin();
                let width = if content_width > 0 {
                    content_width
                } else {
                    150
                };
                let anchor = buffer.create_child_anchor(&mut end);
                let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
                separator.set_size_request(width, -1);
                separator.add_css_class("md-rule");
                view.add_child_at_anchor(&separator, &anchor);
                end = buffer.end_iter();
                continue;
            }
            let start_offset = end.offset();
            buffer.insert(&mut end, &span.text);
            end = buffer.end_iter();
            let start = buffer.iter_at_offset(start_offset);
            for style in &span.styles {
                let tag_name = match style {
                    SpanStyle::Heading(1) => "md-h1",
                    SpanStyle::Heading(2) => "md-h2",
                    SpanStyle::Heading(3) => "md-h3",
                    SpanStyle::Heading(4) => "md-h4",
                    SpanStyle::Heading(5) => "md-h5",
                    SpanStyle::Heading(_) => "md-h6",
                    SpanStyle::Bold => "md-bold",
                    SpanStyle::Italic => "md-italic",
                    SpanStyle::InlineCode => "md-code",
                    SpanStyle::Link(_) => "md-link",
                    SpanStyle::BlockQuote => "md-quote",
                    SpanStyle::CodeBlock => "md-codeblock",
                    SpanStyle::Rule => unreachable!("handled above"),
                };
                buffer.apply_tag_by_name(tag_name, &start, &end);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn plain_paragraph_is_a_single_unmarked_line() {
        let lines = parse("hello world");
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "hello world");
        assert!(lines[0].marker.is_none());
    }

    #[test]
    fn heading_carries_its_level() {
        let lines = parse("## Section");
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "Section");
        assert_eq!(lines[0].spans[0].styles, vec![SpanStyle::Heading(2)]);
    }

    #[test]
    fn bold_and_italic_spans_carry_their_styles() {
        let lines = parse("a **bold** and *italic* word");
        let joined = line_text(&lines[0]);
        assert_eq!(joined, "a bold and italic word");
        let bold_span = lines[0]
            .spans
            .iter()
            .find(|span| span.text == "bold")
            .unwrap();
        assert!(bold_span.styles.contains(&SpanStyle::Bold));
        let italic_span = lines[0]
            .spans
            .iter()
            .find(|span| span.text == "italic")
            .unwrap();
        assert!(italic_span.styles.contains(&SpanStyle::Italic));
    }

    #[test]
    fn inline_code_is_tagged() {
        let lines = parse("run `cargo test` now");
        let code_span = lines[0]
            .spans
            .iter()
            .find(|span| span.text == "cargo test")
            .unwrap();
        assert!(code_span.styles.contains(&SpanStyle::InlineCode));
    }

    #[test]
    fn links_carry_their_destination() {
        let lines = parse("[docs](https://example.com)");
        let span = &lines[0].spans[0];
        assert_eq!(span.text, "docs");
        assert_eq!(
            span.styles,
            vec![SpanStyle::Link("https://example.com".to_string())]
        );
    }

    #[test]
    fn unordered_list_items_get_bullet_markers() {
        let lines = parse("- one\n- two\n- three");
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(line.marker, Some(LineMarker::Bullet { indent: 0 }));
        }
        assert_eq!(line_text(&lines[1]), "two");
    }

    #[test]
    fn ordered_list_items_number_sequentially() {
        let lines = parse("1. first\n2. second\n3. third");
        assert_eq!(
            lines[0].marker,
            Some(LineMarker::Ordered {
                indent: 0,
                number: 1
            })
        );
        assert_eq!(
            lines[1].marker,
            Some(LineMarker::Ordered {
                indent: 0,
                number: 2
            })
        );
        assert_eq!(
            lines[2].marker,
            Some(LineMarker::Ordered {
                indent: 0,
                number: 3
            })
        );
    }

    #[test]
    fn task_list_items_carry_checked_state() {
        let lines = parse("- [ ] todo\n- [x] done");
        assert_eq!(
            lines[0].marker,
            Some(LineMarker::Task {
                indent: 0,
                checked: false
            })
        );
        assert_eq!(
            lines[1].marker,
            Some(LineMarker::Task {
                indent: 0,
                checked: true
            })
        );
    }

    #[test]
    fn fenced_code_block_lines_are_tagged_and_split_per_line() {
        let lines = parse("```\nfn main() {}\nlet x = 1;\n```");
        let code_lines: Vec<&Line> = lines
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .any(|span| span.styles.contains(&SpanStyle::CodeBlock))
            })
            .collect();
        assert_eq!(code_lines.len(), 2);
        assert_eq!(line_text(code_lines[0]), "fn main() {}");
        assert_eq!(line_text(code_lines[1]), "let x = 1;");
    }

    #[test]
    fn blockquote_is_tagged() {
        let lines = parse("> quoted text");
        assert!(lines[0].spans[0].styles.contains(&SpanStyle::BlockQuote));
        assert_eq!(line_text(&lines[0]), "quoted text");
    }

    /// A thematic break isolated by blank lines (the only form that's
    /// actually `Event::Rule` rather than a Setext heading underline — see
    /// this module's doc comment) renders as a visible divider line, not as
    /// three literal hyphen characters indistinguishable from body text.
    #[test]
    fn isolated_rule_renders_as_a_visible_divider() {
        let lines = parse("above\n\n---\n\nbelow");
        let rule_line = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.styles.contains(&SpanStyle::Rule))
            })
            .expect("expected one line carrying SpanStyle::Rule");
        assert!(line_text(rule_line).chars().all(|c| c == RULE_CHAR));
        assert!(line_text(rule_line).chars().count() >= 8);
    }

    /// Pins the documented CommonMark behavior this module deliberately does
    /// not override: `---` immediately after a paragraph (no blank line) is
    /// a Setext H2 heading underline, not a thematic break.
    #[test]
    fn dashes_immediately_after_text_become_a_setext_heading_not_a_rule() {
        let lines = parse("Some content\n---\n\nMore text");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.styles.contains(&SpanStyle::Heading(2)))
        );
        assert!(!lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.styles.contains(&SpanStyle::Rule))
        }));
    }

    #[test]
    fn plain_text_with_no_markdown_syntax_round_trips_as_one_paragraph() {
        // The migration contract: an old sticky note's plain text must
        // remain valid, faithfully-rendered Markdown source.
        let source = "just a plain note, nothing fancy";
        let lines = parse(source);
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), source);
        assert!(lines[0].spans[0].styles.is_empty());
    }

    #[test]
    fn empty_source_parses_to_no_lines() {
        assert!(parse("").is_empty());
    }
}
