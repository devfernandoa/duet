//! The Drawing card (Milestone 7.5): quick freehand sketches next to the
//! agents. A minimal toolbar — pen, eraser, a few colors and widths, undo
//! the last stroke, clear — over a `DrawingArea` that paints the node's
//! vector strokes. The strokes themselves are plain `model::Stroke` data
//! (normalized points, see `drawing.rs`); this widget only edits a copy and
//! reports every finished change through `on_change`, so `app.rs` stays the
//! owner of the persisted record.

use crate::drawing::{self, PEN_COLORS, PEN_WIDTHS};
use crate::model::Stroke;
use crate::node::{
    CollapseHandle, as_drag_handle, resize_handle, spacer, title_buttons, wire_minimize,
};
use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawingTool {
    Pen,
    Eraser,
}

type ChangeCallback = Rc<RefCell<Option<Box<dyn Fn(Vec<Stroke>)>>>>;

#[derive(Clone)]
pub struct DrawingNode {
    pub container: gtk4::Box,
    pub drag_handle: gtk4::Box,
    pub close_button: gtk4::Button,
    pub resize_handle: gtk4::Box,
    pub collapse: CollapseHandle,
    pub area: gtk4::DrawingArea,
    pub clear_button: gtk4::Button,
    empty_hint: gtk4::Label,
    strokes: Rc<RefCell<Vec<Stroke>>>,
    on_change: ChangeCallback,
}

fn tool_toggle(icon: &str, tooltip: &str) -> gtk4::ToggleButton {
    let button = gtk4::ToggleButton::new();
    button.set_icon_name(icon);
    button.add_css_class("flat");
    button.set_tooltip_text(Some(tooltip));
    button.set_focus_on_click(false);
    button
}

fn tool_button(icon: &str, tooltip: &str) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon);
    button.add_css_class("flat");
    button.set_tooltip_text(Some(tooltip));
    button.set_focus_on_click(false);
    button
}

/// A small round color swatch for the toolbar.
fn swatch(color: &str) -> gtk4::ToggleButton {
    let dot = gtk4::DrawingArea::new();
    dot.set_content_width(14);
    dot.set_content_height(14);
    let rgb = drawing::parse_color(color).unwrap_or((0.0, 0.0, 0.0));
    dot.set_draw_func(move |area, cr, width, height| {
        let (w, h) = (width as f64, height as f64);
        cr.arc(
            w / 2.0,
            h / 2.0,
            w.min(h) / 2.0 - 1.0,
            0.0,
            std::f64::consts::TAU,
        );
        cr.set_source_rgb(rgb.0, rgb.1, rgb.2);
        let _ = cr.fill_preserve();
        // A faint ring in the text color keeps a dark ink swatch visible
        // on a dark toolbar.
        let ring = area.color();
        cr.set_source_rgba(
            ring.red() as f64,
            ring.green() as f64,
            ring.blue() as f64,
            0.35,
        );
        cr.set_line_width(1.0);
        let _ = cr.stroke();
    });
    let button = gtk4::ToggleButton::new();
    button.set_child(Some(&dot));
    button.add_css_class("flat");
    button.add_css_class("drawing-swatch");
    button.set_focus_on_click(false);
    button
}

impl DrawingNode {
    pub fn new(
        strokes: Vec<Stroke>,
        collapsed: bool,
        on_collapse_toggle: impl Fn(bool) + 'static,
    ) -> DrawingNode {
        let icon = gtk4::Image::from_icon_name("document-edit-symbolic");
        let title_label = gtk4::Label::new(Some("Drawing"));
        title_label.add_css_class("heading");
        let (minimize_button, close_button) = title_buttons("Remove drawing");
        let title_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        title_bar.add_css_class("node-title-bar");
        title_bar.append(&icon);
        title_bar.append(&title_label);
        title_bar.append(&spacer());
        title_bar.append(&minimize_button);
        title_bar.append(&close_button);
        as_drag_handle(&title_bar);

        let strokes = Rc::new(RefCell::new(
            strokes
                .into_iter()
                .filter(drawing::is_valid)
                .collect::<Vec<_>>(),
        ));
        let tool = Rc::new(Cell::new(DrawingTool::Pen));
        let color = Rc::new(RefCell::new(PEN_COLORS[0].to_string()));
        let width = Rc::new(Cell::new(PEN_WIDTHS[0]));
        let on_change: ChangeCallback = Rc::new(RefCell::new(None));

        // Toolbar: pen | eraser | colors | widths | undo | clear.
        let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        toolbar.add_css_class("drawing-toolbar");
        let pen = tool_toggle("document-edit-symbolic", "Pen");
        let eraser = tool_toggle(
            "edit-clear-symbolic",
            "Eraser — removes the strokes it touches",
        );
        eraser.set_group(Some(&pen));
        pen.set_active(true);
        toolbar.append(&pen);
        toolbar.append(&eraser);
        toolbar.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
        let mut first_swatch: Option<gtk4::ToggleButton> = None;
        for (index, hex) in PEN_COLORS.iter().enumerate() {
            let button = swatch(hex);
            button.set_tooltip_text(Some(["Ink", "Blue", "Red", "Green"][index]));
            if let Some(first) = &first_swatch {
                button.set_group(Some(first));
            } else {
                button.set_active(true);
                first_swatch = Some(button.clone());
            }
            button.connect_toggled({
                let color = Rc::clone(&color);
                let pen = pen.clone();
                let hex = hex.to_string();
                move |button| {
                    if button.is_active() {
                        *color.borrow_mut() = hex.clone();
                        pen.set_active(true);
                    }
                }
            });
            toolbar.append(&button);
        }
        toolbar.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
        let mut first_width: Option<gtk4::ToggleButton> = None;
        for (index, value) in PEN_WIDTHS.iter().enumerate() {
            let label = ["Thin", "Medium", "Thick"][index];
            let button = gtk4::ToggleButton::new();
            let line = gtk4::DrawingArea::new();
            line.set_content_width(16);
            line.set_content_height(14);
            let stroke_width = *value;
            line.set_draw_func(move |area, cr, width, height| {
                let rgba = area.color();
                cr.set_source_rgba(
                    rgba.red() as f64,
                    rgba.green() as f64,
                    rgba.blue() as f64,
                    rgba.alpha() as f64,
                );
                cr.set_line_width(stroke_width.min(height as f64 - 2.0));
                cr.set_line_cap(gtk4::cairo::LineCap::Round);
                cr.move_to(3.0, height as f64 / 2.0);
                cr.line_to(width as f64 - 3.0, height as f64 / 2.0);
                let _ = cr.stroke();
            });
            button.set_child(Some(&line));
            button.add_css_class("flat");
            button.set_tooltip_text(Some(label));
            button.set_focus_on_click(false);
            if let Some(first) = &first_width {
                button.set_group(Some(first));
            } else {
                button.set_active(true);
                first_width = Some(button.clone());
            }
            button.connect_toggled({
                let width = Rc::clone(&width);
                let value = *value;
                move |button| {
                    if button.is_active() {
                        width.set(value);
                    }
                }
            });
            toolbar.append(&button);
        }
        toolbar.append(&spacer());
        let undo_button = tool_button("edit-undo-symbolic", "Undo the last stroke");
        let clear_button = tool_button("edit-clear-all-symbolic", "Clear the drawing…");
        clear_button.add_css_class("destructive-hover");
        toolbar.append(&undo_button);
        toolbar.append(&clear_button);

        let area = gtk4::DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.add_css_class("drawing-surface");
        // A pen stroke is a drag that must never pan the canvas.
        area.add_css_class(crate::canvas::NO_CANVAS_PAN_CLASS);
        {
            let strokes = Rc::clone(&strokes);
            area.set_draw_func(move |_area, cr, width, height| {
                let size = (width as f64, height as f64);
                cr.set_line_cap(gtk4::cairo::LineCap::Round);
                cr.set_line_join(gtk4::cairo::LineJoin::Round);
                for stroke in strokes.borrow().iter() {
                    paint_stroke(cr, stroke, size);
                }
            });
        }
        let empty_hint = gtk4::Label::new(Some("Draw here"));
        empty_hint.add_css_class("dim-label");
        empty_hint.add_css_class("drawing-hint");
        empty_hint.set_can_target(false);
        empty_hint.set_visible(strokes.borrow().is_empty());

        // Drawing and erasing. The gesture claims the press at once, so the
        // canvas never pans and the card never moves while sketching.
        let drag = gtk4::GestureDrag::new();
        drag.set_button(gtk4::gdk::BUTTON_PRIMARY);
        let drawing_now = Rc::new(Cell::new(false));
        let erased_any = Rc::new(Cell::new(false));
        drag.connect_drag_begin({
            let area = area.clone();
            let strokes = Rc::clone(&strokes);
            let tool = Rc::clone(&tool);
            let color = Rc::clone(&color);
            let width = Rc::clone(&width);
            let drawing_now = Rc::clone(&drawing_now);
            let erased_any = Rc::clone(&erased_any);
            let empty_hint = empty_hint.clone();
            move |gesture, x, y| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                let size = (area.width() as f64, area.height() as f64);
                match tool.get() {
                    DrawingTool::Pen => {
                        let mut stroke = Stroke {
                            color: color.borrow().clone(),
                            width: width.get(),
                            points: Vec::new(),
                        };
                        drawing::extend_stroke(&mut stroke, (x, y), size);
                        strokes.borrow_mut().push(stroke);
                        drawing_now.set(true);
                        empty_hint.set_visible(false);
                    }
                    DrawingTool::Eraser => {
                        erased_any
                            .set(drawing::erase_at(&mut strokes.borrow_mut(), (x, y), size) > 0);
                    }
                }
                area.queue_draw();
            }
        });
        drag.connect_drag_update({
            let area = area.clone();
            let strokes = Rc::clone(&strokes);
            let tool = Rc::clone(&tool);
            let drawing_now = Rc::clone(&drawing_now);
            let erased_any = Rc::clone(&erased_any);
            move |gesture, offset_x, offset_y| {
                let Some((start_x, start_y)) = gesture.start_point() else {
                    return;
                };
                let point = (start_x + offset_x, start_y + offset_y);
                let size = (area.width() as f64, area.height() as f64);
                match tool.get() {
                    DrawingTool::Pen if drawing_now.get() => {
                        if let Some(stroke) = strokes.borrow_mut().last_mut() {
                            drawing::extend_stroke(stroke, point, size);
                        }
                    }
                    DrawingTool::Eraser => {
                        if drawing::erase_at(&mut strokes.borrow_mut(), point, size) > 0 {
                            erased_any.set(true);
                        }
                    }
                    _ => return,
                }
                area.queue_draw();
            }
        });
        drag.connect_drag_end({
            let strokes = Rc::clone(&strokes);
            let on_change = Rc::clone(&on_change);
            let drawing_now = Rc::clone(&drawing_now);
            let erased_any = Rc::clone(&erased_any);
            let empty_hint = empty_hint.clone();
            move |_gesture, _x, _y| {
                let changed = drawing_now.replace(false) || erased_any.replace(false);
                if changed {
                    empty_hint.set_visible(strokes.borrow().is_empty());
                    if let Some(callback) = on_change.borrow().as_ref() {
                        callback(strokes.borrow().clone());
                    }
                }
            }
        });
        area.add_controller(drag);

        pen.connect_toggled({
            let tool = Rc::clone(&tool);
            let area = area.clone();
            move |button| {
                if button.is_active() {
                    tool.set(DrawingTool::Pen);
                    area.set_cursor_from_name(Some("crosshair"));
                }
            }
        });
        eraser.connect_toggled({
            let tool = Rc::clone(&tool);
            let area = area.clone();
            move |button| {
                if button.is_active() {
                    tool.set(DrawingTool::Eraser);
                    area.set_cursor_from_name(Some("cell"));
                }
            }
        });
        area.set_cursor_from_name(Some("crosshair"));

        undo_button.connect_clicked({
            let strokes = Rc::clone(&strokes);
            let area = area.clone();
            let on_change = Rc::clone(&on_change);
            let empty_hint = empty_hint.clone();
            move |_| {
                if strokes.borrow_mut().pop().is_some() {
                    area.queue_draw();
                    empty_hint.set_visible(strokes.borrow().is_empty());
                    if let Some(callback) = on_change.borrow().as_ref() {
                        callback(strokes.borrow().clone());
                    }
                }
            }
        });

        let surface = gtk4::Overlay::new();
        surface.set_child(Some(&area));
        surface.add_overlay(&empty_hint);
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content.append(&toolbar);
        content.append(&surface);
        content.set_size_request(280, 200);

        let resize_handle = resize_handle();
        let body = gtk4::Overlay::new();
        body.set_child(Some(&content));
        body.add_overlay(&resize_handle);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&title_bar);
        container.append(&body);
        container.set_css_classes(&["card", "drawing-node"]);

        let collapse = wire_minimize(
            &minimize_button,
            &body,
            &container,
            collapsed,
            on_collapse_toggle,
        );

        DrawingNode {
            container,
            drag_handle: title_bar,
            close_button,
            resize_handle,
            collapse,
            area,
            clear_button,
            empty_hint,
            strokes,
            on_change,
        }
    }

    /// Called with the full stroke list after every finished stroke, erase,
    /// undo or clear.
    pub fn connect_changed(&self, f: impl Fn(Vec<Stroke>) + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(f));
    }

    pub fn strokes(&self) -> Vec<Stroke> {
        self.strokes.borrow().clone()
    }

    /// Shows `strokes` — the record's — without reporting a change: for
    /// undo/redo and clear, where the record was already updated (and may
    /// still be borrowed by the caller).
    pub fn set_strokes(&self, strokes: Vec<Stroke>) {
        if *self.strokes.borrow() == strokes {
            return;
        }
        *self.strokes.borrow_mut() = strokes;
        self.empty_hint
            .set_visible(self.strokes.borrow().is_empty());
        self.area.queue_draw();
    }

    /// Replaces the strokes as a user edit would (a finished stroke, an
    /// erase): shows them and reports them through `connect_changed`.
    pub fn commit_strokes(&self, strokes: Vec<Stroke>) {
        self.set_strokes(strokes);
        if let Some(callback) = self.on_change.borrow().as_ref() {
            callback(self.strokes.borrow().clone());
        }
    }
}

/// Paints one stroke on a surface of `size` pixels.
pub fn paint_stroke(cr: &gtk4::cairo::Context, stroke: &Stroke, size: (f64, f64)) {
    let Some((r, g, b)) = drawing::parse_color(&stroke.color) else {
        return;
    };
    cr.set_source_rgb(r, g, b);
    cr.set_line_width(stroke.width);
    let mut points = stroke.points.iter().map(|p| drawing::denormalize(*p, size));
    let Some(first) = points.next() else {
        return;
    };
    cr.move_to(first.0, first.1);
    let mut any = false;
    for point in points {
        cr.line_to(point.0, point.1);
        any = true;
    }
    if !any {
        // A single tap: a dot.
        cr.line_to(first.0 + 0.01, first.1);
    }
    let _ = cr.stroke();
}
