//! Pure geometry for the canvas's align/distribute commands. Operates on
//! plain `(position, size)` pairs — no `Uuid`, no `NodeRecord`, no GTK — so
//! `app.rs` only has to zip its selected nodes' geometry in, call one of
//! these, and zip the results back out onto the live records/widgets.

/// A node's world-space geometry, as these functions need it.
pub type Geometry = ((f64, f64), (f64, f64));

pub fn align_left(nodes: &mut [Geometry]) {
    let Some(min_x) = nodes
        .iter()
        .map(|(position, _)| position.0)
        .reduce(f64::min)
    else {
        return;
    };
    for (position, _) in nodes.iter_mut() {
        position.0 = min_x;
    }
}

pub fn align_right(nodes: &mut [Geometry]) {
    let Some(max_right) = nodes
        .iter()
        .map(|(position, size)| position.0 + size.0)
        .reduce(f64::max)
    else {
        return;
    };
    for (position, size) in nodes.iter_mut() {
        position.0 = max_right - size.0;
    }
}

pub fn align_top(nodes: &mut [Geometry]) {
    let Some(min_y) = nodes
        .iter()
        .map(|(position, _)| position.1)
        .reduce(f64::min)
    else {
        return;
    };
    for (position, _) in nodes.iter_mut() {
        position.1 = min_y;
    }
}

pub fn align_bottom(nodes: &mut [Geometry]) {
    let Some(max_bottom) = nodes
        .iter()
        .map(|(position, size)| position.1 + size.1)
        .reduce(f64::max)
    else {
        return;
    };
    for (position, size) in nodes.iter_mut() {
        position.1 = max_bottom - size.1;
    }
}

/// Keeps the leftmost and rightmost nodes fixed and spaces every node in
/// between so the horizontal gaps between consecutive edges are equal.
/// A no-op below 3 nodes — there's nothing to redistribute with fewer than
/// one node "in between" two fixed ends.
pub fn distribute_horizontal(nodes: &mut [Geometry]) {
    if nodes.len() < 3 {
        return;
    }
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by(|&a, &b| nodes[a].0.0.total_cmp(&nodes[b].0.0));
    let first_left = nodes[order[0]].0.0;
    let last = nodes[*order.last().expect("checked len >= 3")];
    let last_right = last.0.0 + last.1.0;
    let total_width: f64 = order.iter().map(|&i| nodes[i].1.0).sum();
    let gap_count = (order.len() - 1) as f64;
    let gap = ((last_right - first_left - total_width) / gap_count).max(0.0);

    let mut cursor = first_left;
    for &index in &order {
        nodes[index].0.0 = cursor;
        cursor += nodes[index].1.0 + gap;
    }
}

/// Vertical counterpart of [`distribute_horizontal`].
pub fn distribute_vertical(nodes: &mut [Geometry]) {
    if nodes.len() < 3 {
        return;
    }
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by(|&a, &b| nodes[a].0.1.total_cmp(&nodes[b].0.1));
    let first_top = nodes[order[0]].0.1;
    let last = nodes[*order.last().expect("checked len >= 3")];
    let last_bottom = last.0.1 + last.1.1;
    let total_height: f64 = order.iter().map(|&i| nodes[i].1.1).sum();
    let gap_count = (order.len() - 1) as f64;
    let gap = ((last_bottom - first_top - total_height) / gap_count).max(0.0);

    let mut cursor = first_top;
    for &index in &order {
        nodes[index].0.1 = cursor;
        cursor += nodes[index].1.1 + gap;
    }
}

/// The world-space bounding box `((min_x, min_y), (max_x, max_y))` of a set
/// of node geometries, used to zoom-to-selection/zoom-to-fit. `None` for an
/// empty set.
pub fn bounding_box(nodes: &[Geometry]) -> Option<((f64, f64), (f64, f64))> {
    nodes
        .iter()
        .map(|&(position, size)| (position, (position.0 + size.0, position.1 + size.1)))
        .reduce(|(min, max), (position, bottom_right)| {
            (
                (min.0.min(position.0), min.1.min(position.1)),
                (max.0.max(bottom_right.0), max.1.max(bottom_right.1)),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_left_moves_everything_to_the_leftmost_x() {
        let mut nodes = vec![((10.0, 0.0), (50.0, 50.0)), ((100.0, 20.0), (30.0, 30.0))];
        align_left(&mut nodes);
        assert_eq!(nodes[0].0.0, 10.0);
        assert_eq!(nodes[1].0.0, 10.0);
    }

    #[test]
    fn align_right_lines_up_right_edges() {
        let mut nodes = vec![((0.0, 0.0), (50.0, 10.0)), ((10.0, 0.0), (20.0, 10.0))];
        align_right(&mut nodes);
        // Rightmost edge is max(0+50, 10+20) = 50.
        assert_eq!(nodes[0].0.0 + nodes[0].1.0, 50.0);
        assert_eq!(nodes[1].0.0 + nodes[1].1.0, 50.0);
    }

    #[test]
    fn align_top_and_bottom_mirror_left_and_right_on_the_y_axis() {
        let mut nodes = vec![((0.0, 5.0), (10.0, 10.0)), ((0.0, 50.0), (10.0, 20.0))];
        align_top(&mut nodes);
        assert_eq!(nodes[0].0.1, 5.0);
        assert_eq!(nodes[1].0.1, 5.0);

        let mut nodes = vec![((0.0, 0.0), (10.0, 10.0)), ((0.0, 5.0), (10.0, 20.0))];
        align_bottom(&mut nodes);
        let bottom = nodes[1].0.1 + nodes[1].1.1; // 25.0, the max
        assert_eq!(nodes[0].0.1 + nodes[0].1.1, bottom);
    }

    #[test]
    fn distribute_horizontal_spaces_middle_nodes_evenly() {
        // Three 10-wide nodes spanning x=0..100 overall; the middle one
        // should land such that gaps on both sides are equal.
        let mut nodes = vec![
            ((0.0, 0.0), (10.0, 10.0)),
            ((40.0, 0.0), (10.0, 10.0)),
            ((90.0, 0.0), (10.0, 10.0)),
        ];
        distribute_horizontal(&mut nodes);
        // Total span 0..100 (100), minus 3*10 width = 70 remaining split into
        // 2 gaps of 35 each: node 0 at 0, node 1 at 0+10+35=45, node 2 at
        // 45+10+35=90.
        assert_eq!(nodes[0].0.0, 0.0);
        assert_eq!(nodes[1].0.0, 45.0);
        assert_eq!(nodes[2].0.0, 90.0);
    }

    #[test]
    fn distribute_with_fewer_than_three_nodes_is_a_no_op() {
        let mut nodes = vec![((0.0, 0.0), (10.0, 10.0)), ((5.0, 0.0), (10.0, 10.0))];
        let before = nodes.clone();
        distribute_horizontal(&mut nodes);
        assert_eq!(nodes, before);
    }

    #[test]
    fn bounding_box_covers_every_node() {
        let nodes = vec![((0.0, 0.0), (10.0, 10.0)), ((20.0, -5.0), (5.0, 5.0))];
        let (min, max) = bounding_box(&nodes).unwrap();
        assert_eq!(min, (0.0, -5.0));
        assert_eq!(max, (25.0, 10.0));
    }

    #[test]
    fn bounding_box_of_empty_slice_is_none() {
        assert!(bounding_box(&[]).is_none());
    }
}
