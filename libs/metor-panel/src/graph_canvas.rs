//! Shared canvas primitives for node-and-wire panes.
//!
//! The node editor and the system-graph tile both paint a scrolling grid,
//! draw bezier wires between sockets, and hit-test those wires against the
//! pointer. Those routines are pure geometry — they carry no knowledge of a
//! particular graph's data model — so they live here, parameterized only on
//! the caller's colors and edge-id type. Both panes share the same `screen =
//! graph - viewport` convention and drive these functions from their own
//! render pass.
//!
//! A wire is a *route*: two or more canvas-local points (boundary anchors
//! plus any interior waypoints from the layout engine), rendered as a chain
//! of cubic beziers. Painting and hit-testing both derive their curves from
//! [`route_segments`], so what the pointer feels always matches what the eye
//! sees.

use gpui::{Axis, Bounds, Hsla, PathBuilder, Pixels, Point, Window, point, px};
use smallvec::SmallVec;

/// A wire's canvas-local polyline: source anchor, interior waypoints, target
/// anchor.
pub(crate) type RoutePoints = SmallVec<[Point<Pixels>; 6]>;

/// One cubic bezier piece of a route, in canvas-local f32 coordinates.
struct Segment {
    p0: (f32, f32),
    c1: (f32, f32),
    c2: (f32, f32),
    p1: (f32, f32),
}

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

/// The cubic chain for a route. Endpoint tangents run along the flow axis
/// with the classic outward pull, so a plain two-point route is exactly the
/// familiar S-curve. Interior tangents are Catmull-Rom (parallel to the line
/// between the surrounding points), clamped so a sharp detour elbow can't
/// overshoot its segment.
fn route_segments(points: &[(f32, f32)], flow: Axis) -> SmallVec<[Segment; 4]> {
    let mut segments = SmallVec::new();
    let n = points.len();
    if n < 2 {
        return segments;
    }

    let along_flow = |mag: f32| -> (f32, f32) {
        match flow {
            Axis::Horizontal => (mag, 0.0),
            Axis::Vertical => (0.0, mag),
        }
    };
    let flow_delta = |a: (f32, f32), b: (f32, f32)| -> f32 {
        match flow {
            Axis::Horizontal => b.0 - a.0,
            Axis::Vertical => b.1 - a.1,
        }
    };
    let clamp_len = |v: (f32, f32), max: f32| -> (f32, f32) {
        let len = (v.0 * v.0 + v.1 * v.1).sqrt();
        if len > max && len > f32::EPSILON {
            let s = max / len;
            (v.0 * s, v.1 * s)
        } else {
            v
        }
    };

    for i in 0..n - 1 {
        let (p0, p1) = (points[i], points[i + 1]);
        let seg_len = ((p1.0 - p0.0).powi(2) + (p1.1 - p0.1).powi(2)).sqrt();
        let start = if i == 0 {
            along_flow(flow_delta(p0, p1).abs().max(40.0) * 0.5)
        } else {
            let t = (
                (p1.0 - points[i - 1].0) / 6.0,
                (p1.1 - points[i - 1].1) / 6.0,
            );
            clamp_len(t, seg_len * 0.4)
        };
        let end = if i == n - 2 {
            along_flow(flow_delta(p0, p1).abs().max(40.0) * 0.5)
        } else {
            let t = (
                (points[i + 2].0 - p0.0) / 6.0,
                (points[i + 2].1 - p0.1) / 6.0,
            );
            clamp_len(t, seg_len * 0.4)
        };
        segments.push(Segment {
            p0,
            c1: (p0.0 + start.0, p0.1 + start.1),
            c2: (p1.0 - end.0, p1.1 - end.1),
            p1,
        });
    }
    segments
}

fn to_f32(route: &[Point<Pixels>]) -> SmallVec<[(f32, f32); 6]> {
    route
        .iter()
        .map(|p| (f32::from(p.x), f32::from(p.y)))
        .collect()
}

/// Paint a route (canvas-local points) as a smooth cubic chain, shifted by
/// `canvas_origin` into window space. `dashed` selects the thinner stroke
/// used for drafts and delayed edges.
pub(crate) fn paint_route(
    canvas_origin: Point<Pixels>,
    route: &[Point<Pixels>],
    flow: Axis,
    color: Hsla,
    dashed: bool,
    window: &mut Window,
) {
    let (ox, oy) = (f32::from(canvas_origin.x), f32::from(canvas_origin.y));
    let segments = route_segments(&to_f32(route), flow);
    if segments.is_empty() {
        return;
    }
    let stroke = if dashed { px(1.0) } else { px(1.5) };
    let mut path = PathBuilder::stroke(stroke);
    path.move_to(point(px(ox + segments[0].p0.0), px(oy + segments[0].p0.1)));
    for s in &segments {
        path.cubic_bezier_to(
            point(px(ox + s.p1.0), px(oy + s.p1.1)),
            point(px(ox + s.c1.0), px(oy + s.c1.1)),
            point(px(ox + s.c2.0), px(oy + s.c2.1)),
        );
    }
    if let Ok(p) = path.build() {
        window.paint_path(p, color);
    }
}

/// Paint a plain two-point wire — the drag-draft and fallback shape.
pub(crate) fn paint_bezier(
    canvas_origin: Point<Pixels>,
    source: Point<Pixels>,
    target: Point<Pixels>,
    color: Hsla,
    dashed: bool,
    window: &mut Window,
) {
    let route: RoutePoints = SmallVec::from_slice(&[source, target]);
    paint_route(canvas_origin, &route, Axis::Horizontal, color, dashed, window);
}

/// Find the first edge whose route passes within `HIT_RADIUS` pixels of
/// `pointer` (pointer is in canvas-local coordinates). Each edge gets a cheap
/// bounding-box precheck, then its curve — the same one [`paint_route`]
/// draws — is sampled per segment. Generic over the caller's edge id so both
/// panes can recover their own selection key.
pub(crate) fn hit_test_edges<E: Clone>(
    edges: &[(E, RoutePoints)],
    pointer: Point<Pixels>,
    flow: Axis,
) -> Option<E> {
    const SAMPLES_PER_SEGMENT: usize = 8;
    const HIT_RADIUS: f32 = 6.0;
    let p = (f32::from(pointer.x), f32::from(pointer.y));
    let mut best: Option<(f32, E)> = None;
    for (edge, route) in edges {
        let segments = route_segments(&to_f32(route), flow);
        if segments.is_empty() {
            continue;
        }
        // The curve is contained in the convex hull of its control points, so
        // an inflated box over all of them is a safe reject.
        let mut min = (f32::INFINITY, f32::INFINITY);
        let mut max = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for s in &segments {
            for q in [s.p0, s.c1, s.c2, s.p1] {
                min = (min.0.min(q.0), min.1.min(q.1));
                max = (max.0.max(q.0), max.1.max(q.1));
            }
        }
        if p.0 < min.0 - HIT_RADIUS
            || p.0 > max.0 + HIT_RADIUS
            || p.1 < min.1 - HIT_RADIUS
            || p.1 > max.1 + HIT_RADIUS
        {
            continue;
        }

        let mut min_dist = f32::INFINITY;
        for s in &segments {
            let mut prev = s.p0;
            for k in 1..=SAMPLES_PER_SEGMENT {
                let t = k as f32 / SAMPLES_PER_SEGMENT as f32;
                let q = cubic_bezier_point(t, s.p0, s.c1, s.c2, s.p1);
                min_dist = min_dist.min(point_segment_distance(p, prev, q));
                prev = q;
            }
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
