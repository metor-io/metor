//! Shared canvas primitives for node-and-wire panes.
//!
//! The node editor and the system-graph tile both paint a scrolling grid,
//! draw bezier wires between sockets, and hit-test those wires against the
//! pointer. Those routines are pure geometry — they carry no knowledge of a
//! particular graph's data model — so they live here, parameterized only on
//! the caller's colors and edge-id type. Both panes share the same `screen =
//! graph - viewport` convention and drive these functions from their own
//! render pass.

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Point, Window, point, px};

/// Paint the 24px background grid across `bounds`. The grid is fixed to the
/// canvas (it doesn't scroll) so it reads as a static backdrop rather than a
/// world-space overlay.
pub(crate) fn paint_grid(bounds: Bounds<Pixels>, grid_color: Hsla, window: &mut Window) {
    let step = 24.0_f32;
    let mut path = PathBuilder::stroke(px(0.5));
    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);
    let end_x = origin_x + f32::from(bounds.size.width);
    let end_y = origin_y + f32::from(bounds.size.height);
    let mut x = origin_x;
    while x < end_x {
        path.move_to(point(px(x), bounds.origin.y));
        path.line_to(point(px(x), bounds.origin.y + bounds.size.height));
        x += step;
    }
    let mut y = origin_y;
    while y < end_y {
        path.move_to(point(bounds.origin.x, px(y)));
        path.line_to(point(bounds.origin.x + bounds.size.width, px(y)));
        y += step;
    }
    if let Ok(p) = path.build() {
        window.paint_path(p, grid_color);
    }
}

/// Find the first edge whose bezier passes within `HIT_RADIUS` pixels of
/// `pointer` (pointer is in canvas-local coordinates). Samples each curve at
/// 16 points and uses point-to-segment distance. Generic over the caller's
/// edge id so both panes can recover their own selection key.
pub(crate) fn hit_test_edges<E: Clone>(
    edges: &[(E, Point<Pixels>, Point<Pixels>)],
    pointer: Point<Pixels>,
) -> Option<E> {
    const SAMPLES: usize = 16;
    const HIT_RADIUS: f32 = 6.0;
    let px_pointer = (f32::from(pointer.x), f32::from(pointer.y));
    let mut best: Option<(f32, E)> = None;
    for (edge, s, t) in edges {
        let sx = f32::from(s.x);
        let sy = f32::from(s.y);
        let tx = f32::from(t.x);
        let ty = f32::from(t.y);
        let dx = (tx - sx).abs().max(40.0) * 0.5;
        let c1 = (sx + dx, sy);
        let c2 = (tx - dx, ty);
        let mut prev = (sx, sy);
        let mut min_dist = f32::INFINITY;
        for i in 1..=SAMPLES {
            let t = i as f32 / SAMPLES as f32;
            let p = cubic_bezier_point(t, (sx, sy), c1, c2, (tx, ty));
            min_dist = min_dist.min(point_segment_distance(px_pointer, prev, p));
            prev = p;
        }
        if min_dist <= HIT_RADIUS {
            match &best {
                Some((d, _)) if *d <= min_dist => {}
                _ => best = Some((min_dist, edge.clone())),
            }
        }
    }
    best.map(|(_, e)| e)
}

pub(crate) fn cubic_bezier_point(
    t: f32,
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
) -> (f32, f32) {
    let mt = 1.0 - t;
    let b0 = mt * mt * mt;
    let b1 = 3.0 * mt * mt * t;
    let b2 = 3.0 * mt * t * t;
    let b3 = t * t * t;
    (
        b0 * p0.0 + b1 * p1.0 + b2 * p2.0 + b3 * p3.0,
        b0 * p0.1 + b1 * p1.1 + b2 * p2.1 + b3 * p3.1,
    )
}

pub(crate) fn point_segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let ax = b.0 - a.0;
    let ay = b.1 - a.1;
    let len2 = ax * ax + ay * ay;
    if len2 < 1e-6 {
        let dx = p.0 - a.0;
        let dy = p.1 - a.1;
        return (dx * dx + dy * dy).sqrt();
    }
    let t = ((p.0 - a.0) * ax + (p.1 - a.1) * ay) / len2;
    let t = t.clamp(0.0, 1.0);
    let cx = a.0 + t * ax;
    let cy = a.1 + t * ay;
    let dx = p.0 - cx;
    let dy = p.1 - cy;
    (dx * dx + dy * dy).sqrt()
}

/// Paint a smooth horizontal cubic bezier from `source` to `target` (both in
/// canvas-local coordinates), shifted by `canvas_origin` into window space.
/// `dashed` selects the thinner, dashed stroke used for drafts and delayed
/// edges.
pub(crate) fn paint_bezier(
    canvas_origin: Point<Pixels>,
    source: Point<Pixels>,
    target: Point<Pixels>,
    color: Hsla,
    dashed: bool,
    window: &mut Window,
) {
    // `source` and `target` are in canvas-local coordinates; shift to window.
    let s = point(canvas_origin.x + source.x, canvas_origin.y + source.y);
    let t = point(canvas_origin.x + target.x, canvas_origin.y + target.y);

    // Smooth horizontal cubic with control points pulled outward.
    let sx = f32::from(s.x);
    let tx = f32::from(t.x);
    let dx = (tx - sx).abs().max(40.0) * 0.5;
    let c1 = point(px(sx + dx), s.y);
    let c2 = point(px(tx - dx), t.y);

    let stroke = if dashed { px(1.0) } else { px(1.5) };
    let mut path = PathBuilder::stroke(stroke);
    path.move_to(s);
    path.cubic_bezier_to(t, c1, c2);
    if let Ok(p) = path.build() {
        window.paint_path(p, color);
    }
}
