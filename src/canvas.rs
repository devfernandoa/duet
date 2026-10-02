pub fn world_to_screen(world: (f64, f64), pan: (f64, f64), zoom: f64) -> (f64, f64) {
    ((world.0 + pan.0) * zoom, (world.1 + pan.1) * zoom)
}

pub fn screen_to_world(screen: (f64, f64), pan: (f64, f64), zoom: f64) -> (f64, f64) {
    (screen.0 / zoom - pan.0, screen.1 / zoom - pan.1)
}

#[derive(Debug, Clone, Copy)]
pub struct CanvasState {
    pub pan: (f64, f64),
    pub zoom: f64,
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

use crate::store::SessionRecord;
use gtk4::prelude::*;
use gtk4::{glib, graphene, gsk};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone)]
pub struct Canvas {
    pub overlay: gtk4::Overlay,
    pub fixed: gtk4::Fixed,
    /// Graph-paper backdrop, *behind* the cards. A separate widget from
    /// `links_area` purely for z-order: `gtk4::Overlay` paints its base child
    /// first and each added overlay on top, so the only way to get grid →
    /// cards → link lines is three layers. Drawing the grid in the same
    /// callback as the links painted it over every card.
    pub grid_area: gtk4::DrawingArea,
    /// Link lines, *in front of* the cards — they connect card edges, so a
    /// line that disappeared under a card would read as broken.
    pub links_area: gtk4::DrawingArea,
    pub state: Rc<RefCell<CanvasState>>,
    nodes: Rc<RefCell<Vec<(gtk4::Widget, (f64, f64))>>>,
}

impl Canvas {
    pub fn new() -> Canvas {
        let fixed = gtk4::Fixed::new();
        // Neither drawing area may target: both span the whole canvas, so a
        // targetable one would swallow every click meant for a card.
        let grid_area = gtk4::DrawingArea::new();
        grid_area.set_can_target(false);
        let links_area = gtk4::DrawingArea::new();
        links_area.set_can_target(false);

        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&grid_area));
        overlay.add_overlay(&fixed);
        overlay.add_overlay(&links_area);

        let state = Rc::new(RefCell::new(CanvasState::new()));
        let nodes: Rc<RefCell<Vec<(gtk4::Widget, (f64, f64))>>> = Rc::new(RefCell::new(Vec::new()));

        // GestureDrag's offsets are relative to the drag's start point (not
        // an incremental delta since the last event), so the pan at drag
        // start is captured here and each update sets pan = start + offset
        // rather than accumulating offsets.
        let drag_start_pan = Rc::new(RefCell::new((0.0, 0.0)));
        let drag = gtk4::GestureDrag::new();
        {
            let state = Rc::clone(&state);
            let drag_start_pan = Rc::clone(&drag_start_pan);
            drag.connect_drag_begin(move |_gesture, _start_x, _start_y| {
                *drag_start_pan.borrow_mut() = state.borrow().pan;
            });
        }
        {
            let state = Rc::clone(&state);
            let nodes = Rc::clone(&nodes);
            let fixed = fixed.clone();
            let grid_area = grid_area.clone();
            let drag_start_pan = Rc::clone(&drag_start_pan);
            drag.connect_drag_update(move |_gesture, offset_x, offset_y| {
                let start_pan = *drag_start_pan.borrow();
                let mut state = state.borrow_mut();
                // `pan` is a world-space (pre-scale) quantity — world_to_screen
                // computes (world + pan) * zoom — while offset_x/offset_y are
                // screen-space pixels from the drag gesture. Divide by zoom to
                // convert the screen-space offset back to world space before
                // adding it, so pan speed matches the pointer at any zoom.
                state.pan = (
                    start_pan.0 + offset_x / state.zoom,
                    start_pan.1 + offset_y / state.zoom,
                );
                apply_view(&fixed, &grid_area, &nodes.borrow(), &state);
            });
        }
        fixed.add_controller(drag);

        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        // A terminal consumes its own scrolls for scrollback before this
        // bubble-phase canvas handler sees them. On empty canvas, the wheel
        // reaches here and zooms the spatial view.
        scroll.set_propagation_phase(gtk4::PropagationPhase::Bubble);
        {
            let state = Rc::clone(&state);
            let nodes = Rc::clone(&nodes);
            let fixed = fixed.clone();
            let grid_area = grid_area.clone();
            scroll.connect_scroll(move |controller, _dx, dy| {
                let mut state = state.borrow_mut();
                // Preserve the world-space point under the pointer, rather
                // than scaling from the canvas origin. This makes the card or
                // empty region the user is looking at stay under the cursor.
                let cursor = controller
                    .current_event()
                    .and_then(|event| event.position())
                    .unwrap_or((0.0, 0.0));
                let world = screen_to_world(cursor, state.pan, state.zoom);
                state.zoom *= if dy < 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
                state.clamp_zoom();
                state.pan = (
                    cursor.0 / state.zoom - world.0,
                    cursor.1 / state.zoom - world.1,
                );
                apply_view(&fixed, &grid_area, &nodes.borrow(), &state);
                glib::Propagation::Stop
            });
        }
        fixed.add_controller(scroll);

        {
            let state = Rc::clone(&state);
            grid_area.set_draw_func(move |_area, cairo_ctx, width, height| {
                draw_grid(cairo_ctx, width as f64, height as f64, &state.borrow());
            });
        }

        Canvas {
            overlay,
            fixed,
            grid_area,
            links_area,
            state,
            nodes,
        }
    }

    /// Multiplies the zoom by `ZOOM_STEP` (`steps` positive zooms in,
    /// negative out), the keyboard equivalent of Ctrl+scroll. Wired to
    /// Ctrl+Plus/Ctrl+Minus in `main.rs`, because a modifier+scroll gesture
    /// is not discoverable — the user asked to be able to zoom without ever
    /// finding the one that already existed.
    pub fn zoom_by_steps(&self, steps: i32) {
        let mut state = self.state.borrow_mut();
        state.zoom *= ZOOM_STEP.powi(steps);
        state.clamp_zoom();
        apply_view(&self.fixed, &self.grid_area, &self.nodes.borrow(), &state);
    }

    /// Back to 1:1 at the world origin (Ctrl+0) — the way out of "I zoomed
    /// or panned until I couldn't find my cards".
    pub fn reset_view(&self) {
        let mut state = self.state.borrow_mut();
        *state = CanvasState::new();
        apply_view(&self.fixed, &self.grid_area, &self.nodes.borrow(), &state);
    }

    pub fn add_node(&self, child: &impl IsA<gtk4::Widget>, world_pos: (f64, f64)) {
        self.fixed.put(child, 0.0, 0.0);
        self.nodes
            .borrow_mut()
            .push((child.clone().upcast(), world_pos));
        self.reposition_node(child, world_pos);
    }

    pub fn reposition_node(&self, child: &impl IsA<gtk4::Widget>, world_pos: (f64, f64)) {
        let widget = child.clone().upcast::<gtk4::Widget>();
        {
            let mut nodes = self.nodes.borrow_mut();
            if let Some(entry) = nodes.iter_mut().find(|(w, _)| *w == widget) {
                entry.1 = world_pos;
            }
        }
        apply_transform(&self.fixed, &widget, world_pos, &self.state.borrow());
    }

    /// Moves an existing card to the end of the `Fixed` child order, which
    /// GTK paints last. Called when a card begins moving so the active card
    /// remains visible above every overlapping card throughout the drag.
    pub fn raise_node(&self, child: &impl IsA<gtk4::Widget>) {
        child.insert_before(&self.fixed, None::<&gtk4::Widget>);
    }

    /// Removes a child from both the `Fixed` container and the internal
    /// tracking list used by `retransform_children`. Callers that remove a
    /// node from the canvas (e.g. deleting a session) must use this instead
    /// of calling `fixed.remove` directly, or the tracking list keeps a
    /// strong reference to a widget no longer in the UI (a leak), and future
    /// pan/zoom keeps calling `set_child_transform` on it.
    pub fn remove_node(&self, child: &impl IsA<gtk4::Widget>) {
        self.fixed.remove(child);
        self.nodes.borrow_mut().retain(|(w, _)| w != child.as_ref());
    }

    /// Registers the draw callback for the link lines, which paint on top of
    /// the cards. `anchors` is called on every draw and returns each link's
    /// world-space endpoints plus whether it is currently selected.
    pub fn set_link_lines_source(&self, anchors: impl Fn() -> Vec<LinkLine> + 'static) {
        let state = Rc::clone(&self.state);
        self.links_area
            .set_draw_func(move |_area, cairo_ctx, _width, _height| {
                let state = *state.borrow();
                for link in anchors() {
                    draw_link(cairo_ctx, &link, &state);
                }
            });
    }

    /// Calls `on_click` with the clicked point in *world* coordinates, for
    /// hit-testing things that are drawn rather than built out of widgets
    /// (link lines). Attached to `fixed` rather than to `links_area` because
    /// both drawing areas cover the whole canvas with `can_target` off
    /// precisely so node clicks still work — turning that on would swallow
    /// every click meant for a card. The gesture never claims its sequence,
    /// so `Canvas`'s own pan drag on the same widget is unaffected.
    pub fn connect_background_click(&self, on_click: impl Fn((f64, f64)) + 'static) {
        let state = Rc::clone(&self.state);
        // The pan `GestureDrag` on this same widget never claims its
        // sequence, so a pan also ends in a `released` here. Comparing press
        // and release keeps a pan from registering as a click on whatever
        // happened to be under the cursor when the button went down.
        let pressed_at = Rc::new(RefCell::new((0.0f64, 0.0f64)));
        let click = gtk4::GestureClick::new();
        click.connect_pressed({
            let pressed_at = Rc::clone(&pressed_at);
            move |_gesture, _n_press, x, y| *pressed_at.borrow_mut() = (x, y)
        });
        click.connect_released(move |_gesture, _n_press, x, y| {
            let (press_x, press_y) = *pressed_at.borrow();
            if (x - press_x).hypot(y - press_y) > 4.0 {
                return;
            }
            let state = state.borrow();
            on_click(screen_to_world((x, y), state.pan, state.zoom));
        });
        self.fixed.add_controller(click);
    }
}

/// One link as the canvas needs to draw it: world-space endpoints and
/// whether the user has it selected (clicked once, armed for deletion).
pub struct LinkLine {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub selected: bool,
    /// The two card bounds when they overlap. In that case the ordinary
    /// directional cord is hidden and their exposed outer edges are drawn as
    /// one blue combined shape instead.
    pub overlap: Option<[(f64, f64, f64, f64); 2]>,
}

/// World-space spacing of the faint canvas grid, and of its stronger every-
/// fifth line. The grid exists so panning and zooming are visible at all —
/// an empty canvas gives the eye nothing to measure movement against.
const GRID_MINOR: f64 = 100.0;
const GRID_MAJOR: f64 = 500.0;

/// Multiplicative zoom per step, shared by Ctrl+scroll and Ctrl+Plus/Minus
/// so the two agree on what "one notch" means.
const ZOOM_STEP: f64 = 1.1;

/// The four control points of a link's cubic Bezier, in world space. The
/// handles stick straight out sideways from each card edge, which is what
/// makes a link read as a routed connector rather than a debug line. Public
/// (with `bezier_point`/`distance_to_link`) so hit-testing a click uses the
/// exact same curve that was drawn.
pub fn link_curve(from: (f64, f64), to: (f64, f64)) -> [(f64, f64); 4] {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let reach = (dx.abs().max(dy.abs()) * 0.5).clamp(40.0, 220.0);
    if dx.abs() >= dy.abs() {
        let direction = if dx >= 0.0 { 1.0 } else { -1.0 };
        [
            from,
            (from.0 + direction * reach, from.1),
            (to.0 - direction * reach, to.1),
            to,
        ]
    } else {
        let direction = if dy >= 0.0 { 1.0 } else { -1.0 };
        [
            from,
            (from.0, from.1 + direction * reach),
            (to.0, to.1 - direction * reach),
            to,
        ]
    }
}

pub fn bezier_point(curve: &[(f64, f64); 4], t: f64) -> (f64, f64) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * curve[0].0 + b * curve[1].0 + c * curve[2].0 + d * curve[3].0,
        a * curve[0].1 + b * curve[1].1 + c * curve[2].1 + d * curve[3].1,
    )
}

/// Shortest distance from `point` to a link's curve, in the same (world)
/// units as its endpoints. Approximated by sampling the curve and measuring
/// against the resulting polyline — 24 segments is well under a pixel of
/// error at any zoom this canvas allows, and is a great deal less code than
/// solving for the true nearest point on a cubic.
pub fn distance_to_link(from: (f64, f64), to: (f64, f64), point: (f64, f64)) -> f64 {
    const SAMPLES: usize = 24;
    let curve = link_curve(from, to);
    let mut best = f64::INFINITY;
    let mut previous = curve[0];
    for step in 1..=SAMPLES {
        let next = bezier_point(&curve, step as f64 / SAMPLES as f64);
        best = best.min(distance_to_segment(previous, next, point));
        previous = next;
    }
    best
}

fn distance_to_segment(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length_squared = dx * dx + dy * dy;
    let t = if length_squared <= f64::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length_squared).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).hypot(p.1 - cy)).abs()
}

/// Roughly the height a card's title bar adds above its body. Only used to
/// put a link line's endpoint near the vertical middle of a card rather than
/// its top edge; a few pixels either way is invisible on a link.
pub const TITLE_BAR_HEIGHT: f64 = 28.0;

/// A card's left or right edge at its vertical middle, in world space.
/// `record.position` is the card's top-left and `record.size` is its *body*
/// size, hence the title-bar correction.
pub fn card_edge(record: &SessionRecord, right: bool) -> (f64, f64) {
    (
        if right {
            record.position.0 + record.size.0
        } else {
            record.position.0
        },
        record.position.1 + (record.size.1 + TITLE_BAR_HEIGHT) / 2.0,
    )
}

pub fn card_vertical_edge(record: &SessionRecord, bottom: bool) -> (f64, f64) {
    (
        record.position.0 + record.size.0 / 2.0,
        if bottom {
            record.position.1 + record.size.1 + TITLE_BAR_HEIGHT
        } else {
            record.position.1
        },
    )
}

pub fn card_center(record: &SessionRecord) -> (f64, f64) {
    (
        record.position.0 + record.size.0 / 2.0,
        record.position.1 + (record.size.1 + TITLE_BAR_HEIGHT) / 2.0,
    )
}

pub fn card_intersection(a: &SessionRecord, b: &SessionRecord) -> Option<(f64, f64, f64, f64)> {
    let (a_x, a_y, a_width, a_height) = card_rect(a);
    let (b_x, b_y, b_width, b_height) = card_rect(b);
    let (a_right, a_bottom) = (a_x + a_width, a_y + a_height);
    let (b_right, b_bottom) = (b_x + b_width, b_y + b_height);
    let (left, top) = (a_x.max(b_x), a_y.max(b_y));
    let (right, bottom) = (a_right.min(b_right), a_bottom.min(b_bottom));
    (right > left && bottom > top).then_some((left, top, right - left, bottom - top))
}

pub fn card_rect(record: &SessionRecord) -> (f64, f64, f64, f64) {
    (
        record.position.0,
        record.position.1,
        record.size.0,
        record.size.1 + TITLE_BAR_HEIGHT,
    )
}

/// Graph-paper backdrop, drawn in world space so it pans and scales with the
/// canvas. A flat mid-gray at low alpha reads as a faint tint over both a
/// light and a dark theme background, which avoids needing to detect which
/// one is in use.
fn draw_grid(cairo_ctx: &gtk4::cairo::Context, width: f64, height: f64, state: &CanvasState) {
    // Below a few pixels of screen spacing the minor grid stops being a cue
    // and becomes a moire pattern, so it is simply dropped when zoomed out.
    if GRID_MINOR * state.zoom < 6.0 {
        return;
    }
    let top_left = screen_to_world((0.0, 0.0), state.pan, state.zoom);
    let bottom_right = screen_to_world((width, height), state.pan, state.zoom);
    cairo_ctx.set_line_width(1.0);
    for (step, alpha) in [(GRID_MINOR, 0.10), (GRID_MAJOR, 0.20)] {
        cairo_ctx.set_source_rgba(0.5, 0.5, 0.5, alpha);
        let mut world_x = (top_left.0 / step).floor() * step;
        while world_x <= bottom_right.0 {
            let x = world_to_screen((world_x, 0.0), state.pan, state.zoom)
                .0
                .floor()
                + 0.5;
            cairo_ctx.move_to(x, 0.0);
            cairo_ctx.line_to(x, height);
            world_x += step;
        }
        let mut world_y = (top_left.1 / step).floor() * step;
        while world_y <= bottom_right.1 {
            let y = world_to_screen((0.0, world_y), state.pan, state.zoom)
                .1
                .floor()
                + 0.5;
            cairo_ctx.move_to(0.0, y);
            cairo_ctx.line_to(width, y);
            world_y += step;
        }
        let _ = cairo_ctx.stroke();
    }
}

fn draw_link(cairo_ctx: &gtk4::cairo::Context, link: &LinkLine, state: &CanvasState) {
    if let Some([first, second]) = link.overlap {
        // Drawing only portions of each card border that sit outside its
        // partner produces the outline of their union. That makes a complete
        // overlap read as a single connected shape without a stray cord or a
        // rectangle in the shared area.
        cairo_ctx.set_source_rgba(0.21, 0.52, 0.89, 0.90);
        cairo_ctx.set_line_width(3.0);
        cairo_ctx.set_line_cap(gtk4::cairo::LineCap::Round);
        draw_exposed_rect(cairo_ctx, first, second, state);
        draw_exposed_rect(cairo_ctx, second, first, state);
        let _ = cairo_ctx.stroke();
        return;
    }
    // A cubic Bezier is affine-invariant, so transforming the four control
    // points is the same as transforming the curve.
    let curve = link_curve(link.from, link.to);
    let screen = curve.map(|point| world_to_screen(point, state.pan, state.zoom));

    // Selected links take the same orange as a destructive action elsewhere
    // in the GNOME palette, because the next click on one deletes it.
    let (red, green, blue) = if link.selected {
        (0.90, 0.38, 0.0)
    } else {
        (0.21, 0.52, 0.89)
    };
    let width = if link.selected { 3.5 } else { 2.0 };

    cairo_ctx.set_line_cap(gtk4::cairo::LineCap::Round);
    cairo_ctx.set_source_rgba(red, green, blue, if link.selected { 1.0 } else { 0.85 });
    cairo_ctx.set_line_width(width);
    cairo_ctx.move_to(screen[0].0, screen[0].1);
    cairo_ctx.curve_to(
        screen[1].0,
        screen[1].1,
        screen[2].0,
        screen[2].1,
        screen[3].0,
        screen[3].1,
    );
    let _ = cairo_ctx.stroke();

    // A dot at the source end and an arrowhead at the target end show the
    // direction selected when the logical connection was created.
    let _ = cairo_ctx.arc(
        screen[0].0,
        screen[0].1,
        width * 1.6,
        0.0,
        std::f64::consts::TAU,
    );
    let _ = cairo_ctx.fill();

    let (dx, dy) = (screen[3].0 - screen[2].0, screen[3].1 - screen[2].1);
    let length = dx.hypot(dy);
    if length > f64::EPSILON {
        let (ux, uy) = (dx / length, dy / length);
        let head = width * 4.0;
        cairo_ctx.move_to(screen[3].0, screen[3].1);
        cairo_ctx.line_to(
            screen[3].0 - head * ux + head * 0.45 * uy,
            screen[3].1 - head * uy - head * 0.45 * ux,
        );
        cairo_ctx.line_to(
            screen[3].0 - head * ux - head * 0.45 * uy,
            screen[3].1 - head * uy + head * 0.45 * ux,
        );
        cairo_ctx.close_path();
        let _ = cairo_ctx.fill();
    }
}

/// Adds the parts of `rect`'s border that do not lie inside `other` to the
/// current Cairo path. Both rectangles are world-space `(x, y, width, height)`
/// bounds; the transform is applied once per visible segment.
fn draw_exposed_rect(
    cairo_ctx: &gtk4::cairo::Context,
    rect: (f64, f64, f64, f64),
    other: (f64, f64, f64, f64),
    state: &CanvasState,
) {
    let (x, y, width, height) = rect;
    let (other_x, other_y, other_width, other_height) = other;
    let (right, bottom) = (x + width, y + height);
    let (other_right, other_bottom) = (other_x + other_width, other_y + other_height);

    let horizontal = |edge_y: f64, context: &gtk4::cairo::Context| {
        if edge_y > other_y && edge_y < other_bottom {
            // Clamp both cut points to this edge. Without that, a card fully
            // inside another can produce a backwards segment across the
            // opposite corner of the card.
            let before_end = other_x.clamp(x, right);
            let after_start = other_right.clamp(x, right);
            draw_world_segment(context, (x, edge_y), (before_end, edge_y), state);
            draw_world_segment(context, (after_start, edge_y), (right, edge_y), state);
        } else {
            draw_world_segment(context, (x, edge_y), (right, edge_y), state);
        }
    };
    let vertical = |edge_x: f64, context: &gtk4::cairo::Context| {
        if edge_x > other_x && edge_x < other_right {
            let before_end = other_y.clamp(y, bottom);
            let after_start = other_bottom.clamp(y, bottom);
            draw_world_segment(context, (edge_x, y), (edge_x, before_end), state);
            draw_world_segment(context, (edge_x, after_start), (edge_x, bottom), state);
        } else {
            draw_world_segment(context, (edge_x, y), (edge_x, bottom), state);
        }
    };

    horizontal(y, cairo_ctx);
    horizontal(bottom, cairo_ctx);
    vertical(x, cairo_ctx);
    vertical(right, cairo_ctx);
}

fn draw_world_segment(
    cairo_ctx: &gtk4::cairo::Context,
    from: (f64, f64),
    to: (f64, f64),
    state: &CanvasState,
) {
    if from == to {
        return;
    }
    let from = world_to_screen(from, state.pan, state.zoom);
    let to = world_to_screen(to, state.pan, state.zoom);
    cairo_ctx.move_to(from.0, from.1);
    cairo_ctx.line_to(to.0, to.1);
}

fn apply_transform(
    fixed: &gtk4::Fixed,
    child: &gtk4::Widget,
    world_pos: (f64, f64),
    state: &CanvasState,
) {
    let screen = world_to_screen(world_pos, state.pan, state.zoom);
    let transform = gsk::Transform::new()
        .translate(&graphene::Point::new(screen.0 as f32, screen.1 as f32))
        .scale(state.zoom as f32, state.zoom as f32);
    fixed.set_child_transform(child, Some(&transform));
}

/// Re-applies every tracked child's transform after a pan/zoom change, using
/// each child's known world position (tracked in `Canvas::nodes` as of
/// `add_node`/`reposition_node`) rather than its current screen transform —
/// this keeps both pan and zoom exact, including zoom re-centering on the
/// canvas origin rather than the pointer (v1: simplest thing that works).
/// Also repaints the grid, which is drawn in world space and so is only ever
/// stale for exactly these changes — `reposition_node` moves one card and
/// deliberately does not come through here.
fn apply_view(
    fixed: &gtk4::Fixed,
    grid_area: &gtk4::DrawingArea,
    nodes: &[(gtk4::Widget, (f64, f64))],
    state: &CanvasState,
) {
    for (child, world_pos) in nodes {
        apply_transform(fixed, child, *world_pos, state);
    }
    grid_area.queue_draw();
}

/// A node body's real current size in pre-zoom pixels (GTK allocations are in
/// the widget's own untransformed space, so this is directly comparable to the
/// `size` stored in a record), or `None` if it isn't allocated yet.
///
/// Resize drags use this rather than `record.size` as their starting point,
/// which is the fix for the "card shrank and can't be dragged back up" state.
/// The two quantities drift apart, because a card's rendered width is
/// `max(record.size.0, title-bar minimum)`: a 28-character session name
/// measured 348px of title bar, so a card whose record said 220 actually drew
/// 348 wide. Starting a resize from the stale 220 meant the first 128px of
/// rightward drag changed nothing visible at all, and every fresh drag
/// restarted from the same stale number — the card read as un-growable. The
/// title label is now ellipsized (see `SessionNode::new`) so the title bar
/// stops forcing a minimum, but anchoring the drag to the real size is what
/// makes the gesture self-correcting regardless of where a mismatch comes
/// from (grid rounding, a future wider title bar, a restored record).
pub fn allocated_size(widget: &impl IsA<gtk4::Widget>) -> Option<(f64, f64)> {
    let widget = widget.as_ref();
    let (width, height) = (widget.width(), widget.height());
    (width > 0 && height > 0).then_some((width as f64, height as f64))
}

/// Expresses `local` — a point in the coordinate space of the widget
/// `gesture` is attached to — in the canvas `Fixed`'s coordinate space.
pub fn canvas_point(
    gesture: &gtk4::GestureDrag,
    fixed: &gtk4::Fixed,
    local: (f64, f64),
) -> Option<(f64, f64)> {
    let widget = gesture.widget()?;
    widget
        .compute_point(
            fixed,
            &gtk4::graphene::Point::new(local.0 as f32, local.1 as f32),
        )
        .map(|point| (point.x() as f64, point.y() as f64))
}

/// How far the pointer has moved, in world units, since the drag began.
/// `start_pointer` is `canvas_point` of the gesture's start point, captured
/// once in the `drag-begin` handler.
///
/// Why this isn't just `offset / zoom`: a `GestureDrag`'s `offset_x`/
/// `offset_y` are expressed in the coordinate space of the widget the
/// gesture is attached to, and GTK re-translates the pointer through that
/// widget's *current* transform on every event. Every gesture here is
/// attached to chrome inside a card whose transform the handler changes on
/// every tick, so the card's own displacement feeds straight back into the
/// reported offset: with displacement `d` applied and the pointer `m` from
/// where it started, GTK reports `offset = m - d`, so assigning `d = offset`
/// settles at `d = m / 2` — the card tracks at half the cursor's speed. The
/// same recurrence `d_n = m_n - d_(n-1)` has gain -1, so it never damps:
/// every pointer-sampling irregularity adds a non-decaying alternating
/// wobble, which is the jitter that grew the further a card was dragged.
///
/// Mapping both the start point and the current point into the canvas
/// `Fixed`'s space cancels the card's displacement exactly and leaves the
/// true pointer movement. `Fixed` is the right reference because it never
/// moves — pan and zoom only change its *children's* transforms, which is
/// why `Canvas`'s own pan gesture (attached to `fixed` itself) never had
/// this problem. It also makes the `/ zoom` correct: `Fixed`-space units are
/// screen pixels, whereas the raw gesture offsets were already in the card's
/// own zoom-scaled space and so were being divided by zoom a second time.
pub fn world_drag_delta(
    gesture: &gtk4::GestureDrag,
    fixed: &gtk4::Fixed,
    start_pointer: (f64, f64),
    zoom: f64,
) -> Option<(f64, f64)> {
    let (start_x, start_y) = gesture.start_point()?;
    let (offset_x, offset_y) = gesture.offset()?;
    let now = canvas_point(gesture, fixed, (start_x + offset_x, start_y + offset_y))?;
    Some((
        (now.0 - start_pointer.0) / zoom,
        (now.1 - start_pointer.1) / zoom,
    ))
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

    /// The whole point of hit-testing a link is that a click near the curve
    /// counts and a click away from it does not, so both halves are checked
    /// against the curve's actual bulge rather than the straight line.
    #[test]
    fn distance_to_link_measures_against_the_drawn_curve() {
        let (from, to) = ((0.0, 0.0), (400.0, 0.0));
        // Both endpoints and the midpoint of a horizontal link lie on it.
        assert!(distance_to_link(from, to, (0.0, 0.0)) < 0.01);
        assert!(distance_to_link(from, to, (400.0, 0.0)) < 0.01);
        assert!(distance_to_link(from, to, (200.0, 0.0)) < 0.01);
        // Well off the line.
        assert!(distance_to_link(from, to, (200.0, 90.0)) > 80.0);

        // A link between cards at different heights bows out sideways, so
        // its own midpoint must register as on the curve while the straight
        // chord's midpoint need not.
        let (from, to) = ((0.0, 0.0), (300.0, 200.0));
        let middle = bezier_point(&link_curve(from, to), 0.5);
        assert!(distance_to_link(from, to, middle) < 0.5);
    }

    #[test]
    fn link_curve_handles_reach_sideways_from_each_end() {
        let curve = link_curve((0.0, 10.0), (500.0, 90.0));
        assert_eq!(curve[0], (0.0, 10.0));
        assert_eq!(curve[3], (500.0, 90.0));
        // Control points share their endpoint's y, which is what makes the
        // curve leave the source edge horizontally and arrive likewise.
        assert_eq!(curve[1].1, 10.0);
        assert_eq!(curve[2].1, 90.0);
        assert!(curve[1].0 > curve[0].0 && curve[2].0 < curve[3].0);
        let reversed = link_curve((500.0, 10.0), (0.0, 90.0));
        assert!(reversed[1].0 < reversed[0].0 && reversed[2].0 > reversed[3].0);
        let vertical = link_curve((10.0, 0.0), (90.0, 500.0));
        assert_eq!(vertical[1].0, 10.0);
        assert_eq!(vertical[2].0, 90.0);
        assert!(vertical[1].1 > vertical[0].1 && vertical[2].1 < vertical[3].1);
        // Even for coincident endpoints the handles stay finite and apart.
        let degenerate = link_curve((5.0, 5.0), (5.0, 5.0));
        assert_eq!(degenerate[1].0, 45.0);
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
