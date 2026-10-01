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
