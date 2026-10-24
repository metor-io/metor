//! Dependency leaders between timeline bars.
//!
//! A leader is a polyline routed through right-angle elbows. Painting and
//! hit-testing both walk [`orthogonal_points`], so what the pointer feels
//! always matches what the eye sees. Callers work in canvas-local
//! coordinates and pass the canvas origin in.

use gpui::{Hsla, PathBuilder, Pixels, Point, Window, point, px};
use smallvec::SmallVec;

/// Stroke appearance of one leader.
#[derive(Debug, Clone, Copy)]
pub(super) struct LineStyle {
    pub width: Pixels,
    pub dashed: bool,
}

/// Dash pattern for a dashed leader: long enough to read as intent
/// (delayed) rather than as a rendering artifact.
const DASH: [Pixels; 2] = [px(6.0), px(4.0)];

/// How near the pointer must be to grab a leader.
pub(super) const LINE_HIT_RADIUS: f32 = 6.0;

/// Expand a polyline into right-angle elbows.
///
/// Each leg turns at its midpoint on the dominant axis, giving the Z-shaped
/// run a schematic wants rather than the L an "all horizontal then all
/// vertical" rule produces — an L crowds one endpoint and reads as though the
/// line belongs to it. Legs already axis-aligned gain collinear points, which
/// paint and hit-test identically.
fn orthogonal_points(points: &[Point<Pixels>]) -> SmallVec<[Point<Pixels>; 12]> {
    let mut out: SmallVec<[Point<Pixels>; 12]> = SmallVec::new();
    let Some(first) = points.first() else {
        return out;
    };
    out.push(*first);
    for leg in points.windows(2) {
        let (a, b) = (leg[0], leg[1]);
        if (b.x - a.x).abs() >= (b.y - a.y).abs() {
            let mid = a.x + (b.x - a.x) / 2.0;
            out.push(point(mid, a.y));
            out.push(point(mid, b.y));
        } else {
            let mid = a.y + (b.y - a.y) / 2.0;
            out.push(point(a.x, mid));
            out.push(point(b.x, mid));
        }
        out.push(b);
    }
    out
}

/// Paint a leader between two or more canvas-local points.
pub(super) fn paint_line(
    canvas_origin: Point<Pixels>,
    points: &[Point<Pixels>],
    style: LineStyle,
    color: Hsla,
    window: &mut Window,
) {
    if points.len() < 2 {
        return;
    }
    let shifted: SmallVec<[Point<Pixels>; 12]> = points
        .iter()
        .map(|p| point(canvas_origin.x + p.x, canvas_origin.y + p.y))
        .collect();
    let drawn = orthogonal_points(&shifted);
    let mut path = PathBuilder::stroke(style.width);
    if style.dashed {
        path = path.dash_array(&DASH);
    }
    path.move_to(drawn[0]);
    for p in &drawn[1..] {
        path.line_to(*p);
    }
    if let Ok(p) = path.build() {
        window.paint_path(p, color);
    }
}

/// Distance from `pointer` to a leader, both in canvas-local coordinates.
pub(super) fn distance_to_line(points: &[Point<Pixels>], pointer: Point<Pixels>) -> f32 {
    let samples = orthogonal_points(points);
    if samples.len() < 2 {
        return f32::INFINITY;
    }
    let p = (f32::from(pointer.x), f32::from(pointer.y));
    samples
        .windows(2)
        .map(|w| {
            point_segment_distance(
                p,
                (f32::from(w[0].x), f32::from(w[0].y)),
                (f32::from(w[1].x), f32::from(w[1].y)),
            )
        })
        .fold(f32::INFINITY, f32::min)
}

fn point_segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
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
