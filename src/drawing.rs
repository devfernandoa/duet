//! Geometry for Drawing nodes (Milestone 7.5), GTK-free so it is testable
//! and the persisted shape never depends on a widget.
//!
//! Strokes are stored with points normalized to the drawing surface
//! (`0.0..=1.0` on both axes): a point is divided by the surface size when
//! drawn and multiplied back when painted. Resizing a Drawing therefore
//! stretches the sketch with the card — predictable, and it can never clip
//! or corrupt a stroke. Stroke *width* stays in pixels (at 100% zoom) so a
//! line doesn't get fat just because the card grew.

use crate::model::Stroke;

/// Pen colors offered by the toolbar: ink, blue, red, green.
pub const PEN_COLORS: [&str; 4] = ["#241f31", "#1c71d8", "#e01b24", "#26a269"];

/// Pen widths offered by the toolbar (pixels at 100% zoom).
pub const PEN_WIDTHS: [f64; 3] = [2.0, 4.0, 8.0];

/// How close (in pixels) the eraser must come to a stroke to remove it.
pub const ERASER_RADIUS: f64 = 10.0;

/// Points closer than this (in pixels) to the previous one are dropped
/// while drawing, keeping persisted strokes small.
pub const MIN_POINT_DISTANCE: f64 = 1.5;

/// Pixel `(x, y)` on a surface of `size` → normalized, clamped to the
/// surface. A degenerate (zero) size maps everything to the origin rather
/// than dividing by zero.
pub fn normalize(point: (f64, f64), size: (f64, f64)) -> (f64, f64) {
    let axis = |value: f64, extent: f64| {
        if extent > 0.0 && value.is_finite() {
            (value / extent).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    (axis(point.0, size.0), axis(point.1, size.1))
}

/// Normalized → pixel on a surface of `size`.
pub fn denormalize(point: (f64, f64), size: (f64, f64)) -> (f64, f64) {
    (point.0 * size.0, point.1 * size.1)
}

/// Appends `point` (pixels) to `stroke` unless it is within
/// `MIN_POINT_DISTANCE` of the stroke's last point. Returns whether it was
/// added.
pub fn extend_stroke(stroke: &mut Stroke, point: (f64, f64), size: (f64, f64)) -> bool {
    if let Some(last) = stroke.points.last() {
        let last = denormalize(*last, size);
        if (point.0 - last.0).hypot(point.1 - last.1) < MIN_POINT_DISTANCE {
            return false;
        }
    }
    stroke.points.push(round_point(normalize(point, size)));
    true
}

/// Five decimal places is far below a pixel on any card size, and keeps
/// the persisted JSON compact.
fn round_point(point: (f64, f64)) -> (f64, f64) {
    let round = |value: f64| (value * 100_000.0).round() / 100_000.0;
    (round(point.0), round(point.1))
}

fn distance_to_segment(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length_squared = dx * dx + dy * dy;
    let t = if length_squared <= f64::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length_squared).clamp(0.0, 1.0)
    };
    (p.0 - (a.0 + t * dx)).hypot(p.1 - (a.1 + t * dy))
}

/// Whether the eraser at `point` (pixels) touches `stroke` on a surface of
/// `size` — measured in pixels, so erasing feels the same at any card size.
pub fn stroke_hit(stroke: &Stroke, point: (f64, f64), size: (f64, f64), radius: f64) -> bool {
    let reach = radius + stroke.width / 2.0;
    let pixels: Vec<(f64, f64)> = stroke
        .points
        .iter()
        .map(|p| denormalize(*p, size))
        .collect();
    match pixels.as_slice() {
        [] => false,
        [only] => (only.0 - point.0).hypot(only.1 - point.1) <= reach,
        _ => pixels
            .windows(2)
            .any(|pair| distance_to_segment(pair[0], pair[1], point) <= reach),
    }
}

/// Removes every stroke the eraser at `point` touches; returns how many.
pub fn erase_at(strokes: &mut Vec<Stroke>, point: (f64, f64), size: (f64, f64)) -> usize {
    let before = strokes.len();
    strokes.retain(|stroke| !stroke_hit(stroke, point, size, ERASER_RADIUS));
    before - strokes.len()
}

/// Whether a persisted stroke is usable: a known-good color, a sane width
/// and points inside the unit square. Used to drop damaged strokes on load
/// rather than paint garbage.
pub fn is_valid(stroke: &Stroke) -> bool {
    stroke.width.is_finite()
        && stroke.width > 0.0
        && stroke.width <= 64.0
        && parse_color(&stroke.color).is_some()
        && !stroke.points.is_empty()
        && stroke.points.iter().all(|(x, y)| {
            x.is_finite() && y.is_finite() && (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y)
        })
}

/// `#rrggbb` → (r, g, b) in `0.0..=1.0`.
pub fn parse_color(color: &str) -> Option<(f64, f64, f64)> {
    let hex = color.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[range], 16)
            .ok()
            .map(|v| v as f64 / 255.0)
    };
    Some((channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(points: &[(f64, f64)]) -> Stroke {
        Stroke {
            color: PEN_COLORS[0].to_string(),
            width: 2.0,
            points: points.to_vec(),
        }
    }

    #[test]
    fn normalize_and_denormalize_round_trip_and_clamp() {
        let size = (400.0, 200.0);
        let n = normalize((100.0, 50.0), size);
        assert_eq!(n, (0.25, 0.25));
        assert_eq!(denormalize(n, size), (100.0, 50.0));
        // Outside the surface clamps to its edge; a zero size never divides.
        assert_eq!(normalize((-5.0, 900.0), size), (0.0, 1.0));
        assert_eq!(normalize((10.0, 10.0), (0.0, 0.0)), (0.0, 0.0));
        assert_eq!(normalize((f64::NAN, 10.0), size).0, 0.0);
    }

    #[test]
    fn resizing_scales_strokes_without_changing_the_stored_points() {
        let mut s = stroke(&[]);
        extend_stroke(&mut s, (40.0, 20.0), (400.0, 200.0));
        extend_stroke(&mut s, (360.0, 180.0), (400.0, 200.0));
        let stored = s.points.clone();
        // Twice as big: the same stroke paints twice as far, stored data
        // is untouched (a resize never rewrites strokes).
        let big: Vec<_> = s
            .points
            .iter()
            .map(|p| denormalize(*p, (800.0, 400.0)))
            .collect();
        assert_eq!(big, vec![(80.0, 40.0), (720.0, 360.0)]);
        assert_eq!(s.points, stored);
        assert!(is_valid(&s));
    }

    #[test]
    fn extend_stroke_drops_points_that_barely_moved() {
        let mut s = stroke(&[]);
        let size = (100.0, 100.0);
        assert!(extend_stroke(&mut s, (10.0, 10.0), size));
        assert!(!extend_stroke(&mut s, (10.5, 10.5), size));
        assert!(extend_stroke(&mut s, (20.0, 10.0), size));
        assert_eq!(s.points.len(), 2);
    }

    #[test]
    fn eraser_removes_only_strokes_it_touches() {
        let size = (200.0, 200.0);
        let mut strokes = vec![
            stroke(&[(0.1, 0.1), (0.9, 0.1)]), // horizontal line at y=20px
            stroke(&[(0.1, 0.9), (0.9, 0.9)]), // horizontal line at y=180px
            stroke(&[(0.5, 0.5)]),             // a dot in the middle
        ];
        assert_eq!(erase_at(&mut strokes, (100.0, 100.0), size), 1);
        assert_eq!(strokes.len(), 2);
        assert_eq!(erase_at(&mut strokes, (100.0, 60.0), size), 0);
        assert_eq!(erase_at(&mut strokes, (100.0, 25.0), size), 1);
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].points[0], (0.1, 0.9));
    }

    #[test]
    fn damaged_strokes_are_recognized() {
        assert!(!is_valid(&stroke(&[])));
        assert!(!is_valid(&stroke(&[(1.5, 0.0)])));
        let mut bad_color = stroke(&[(0.1, 0.1)]);
        bad_color.color = "red".to_string();
        assert!(!is_valid(&bad_color));
        let mut bad_width = stroke(&[(0.1, 0.1)]);
        bad_width.width = f64::INFINITY;
        assert!(!is_valid(&bad_width));
        assert_eq!(parse_color("#ff0000"), Some((1.0, 0.0, 0.0)));
        assert_eq!(parse_color("#ff00"), None);
    }
}
