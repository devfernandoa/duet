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

use gtk4::prelude::*;
use gtk4::{gdk, glib, graphene, gsk};
use std::cell::RefCell;
use std::rc::Rc;

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
        // Capture, not the default bubble phase: a card's VTE terminal handles
        // scroll itself (scrollback) and stops the event, so in the bubble
        // phase `fixed` — an ancestor of every card — only ever saw a scroll
        // that happened over empty canvas. Ctrl+scroll therefore did nothing
        // wherever there was something to zoom. Capture runs root-to-target,
        // so this sees the scroll first and still returns `Proceed` for a
        // plain scroll, leaving the terminal's own scrollback intact.
        scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let state = Rc::clone(&state);
            let nodes = Rc::clone(&nodes);
            let fixed = fixed.clone();
            let grid_area = grid_area.clone();
            scroll.connect_scroll(move |controller, _dx, dy| {
                if controller
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK)
                {
                    let mut state = state.borrow_mut();
                    state.zoom *= if dy < 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
                    state.clamp_zoom();
                    apply_view(&fixed, &grid_area, &nodes.borrow(), &state);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
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
        self.links_area.set_draw_func(move |_area, cairo_ctx, _width, _height| {
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
    let reach = ((to.0 - from.0).abs() * 0.5).clamp(40.0, 220.0);
    [
        from,
        (from.0 + reach, from.1),
        (to.0 - reach, to.1),
        to,
    ]
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
            let x = world_to_screen((world_x, 0.0), state.pan, state.zoom).0.floor() + 0.5;
            cairo_ctx.move_to(x, 0.0);
            cairo_ctx.line_to(x, height);
            world_x += step;
        }
        let mut world_y = (top_left.1 / step).floor() * step;
        while world_y <= bottom_right.1 {
            let y = world_to_screen((0.0, world_y), state.pan, state.zoom).1.floor() + 0.5;
            cairo_ctx.move_to(0.0, y);
            cairo_ctx.line_to(width, y);
            world_y += step;
        }
        let _ = cairo_ctx.stroke();
    }
}

fn draw_link(cairo_ctx: &gtk4::cairo::Context, link: &LinkLine, state: &CanvasState) {
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
        screen[1].0, screen[1].1,
        screen[2].0, screen[2].1,
        screen[3].0, screen[3].1,
    );
    let _ = cairo_ctx.stroke();

    // A dot at the source end and an arrowhead at the target end: the link
    // is directional (the source's output feeds the target's input) and a
    // plain line said nothing about which way the bytes flow.
    let _ = cairo_ctx.arc(screen[0].0, screen[0].1, width * 1.6, 0.0, std::f64::consts::TAU);
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
