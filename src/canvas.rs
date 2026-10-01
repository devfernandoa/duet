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
    pub drawing_area: gtk4::DrawingArea,
    pub state: Rc<RefCell<CanvasState>>,
    nodes: Rc<RefCell<Vec<(gtk4::Widget, (f64, f64))>>>,
}

impl Canvas {
    pub fn new() -> Canvas {
        let fixed = gtk4::Fixed::new();
        let drawing_area = gtk4::DrawingArea::new();
        drawing_area.set_can_target(false); // let clicks fall through to nodes

        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&fixed));
        overlay.add_overlay(&drawing_area);

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
            let drag_start_pan = Rc::clone(&drag_start_pan);
            drag.connect_drag_update(move |_gesture, offset_x, offset_y| {
                let start_pan = *drag_start_pan.borrow();
                let mut state = state.borrow_mut();
                state.pan = (start_pan.0 + offset_x, start_pan.1 + offset_y);
                retransform_children(&fixed, &nodes.borrow(), &state);
            });
        }
        fixed.add_controller(drag);

        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        {
            let state = Rc::clone(&state);
            let nodes = Rc::clone(&nodes);
            let fixed = fixed.clone();
            scroll.connect_scroll(move |controller, _dx, dy| {
                if controller
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK)
                {
                    let mut state = state.borrow_mut();
                    state.zoom *= if dy < 0.0 { 1.1 } else { 0.9 };
                    state.clamp_zoom();
                    retransform_children(&fixed, &nodes.borrow(), &state);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        fixed.add_controller(scroll);

        Canvas {
            overlay,
            fixed,
            drawing_area,
            state,
            nodes,
        }
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

/// Re-applies every tracked child's transform after pan/zoom changes, using
/// each child's known world position (tracked in `Canvas::nodes` as of
/// `add_node`/`reposition_node`) rather than its current screen transform —
/// this keeps both pan and zoom exact, including zoom re-centering on the
/// canvas origin rather than the pointer (v1: simplest thing that works).
fn retransform_children(fixed: &gtk4::Fixed, nodes: &[(gtk4::Widget, (f64, f64))], state: &CanvasState) {
    for (child, world_pos) in nodes {
        apply_transform(fixed, child, *world_pos, state);
    }
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
