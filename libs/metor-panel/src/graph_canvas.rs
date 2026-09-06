//! Shared line primitives for the panes that draw connectors.
//!
//! The dashboard's schematic connectors and the execution timeline's
//! dependency leaders both need the same thing: a polyline turned into a
//! stroke, an arrowhead at its tip, and a distance from the pointer that
//! matches what was painted. That is pure geometry — it carries no knowledge
//! of a particular pane's data model — so it lives here, parameterized only on
//! the caller's colors. Callers work in canvas-local coordinates and pass the
//! canvas origin in.
//!
//! [`LineShape`] picks between the curve, the straight run, and the
//! right-angle routing a P&ID or a circuit diagram wants. Painting and
//! hit-testing both derive their samples from [`drawn_polyline`], so what the
//! pointer feels always matches what the eye sees.

use gpui::{Axis, Hsla, PathBuilder, Pixels, Point, Window, point, px};
use smallvec::SmallVec;

// `LineShape` lives with the connector config it is serialized as part of,
// rather than here, so it stays nameable from outside the crate.
pub(crate) use crate::views::dashboard::connectors::LineShape;

/// Stroke appearance shared by every connector.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LineStyle {
    pub width: Pixels,
    pub dashed: bool,
    pub shape: LineShape,
}

/// Dash pattern for a dashed connector: long enough to read as intent
/// (planned, inactive) rather than as a rendering artifact.
const DASH: [Pixels; 2] = [px(6.0), px(4.0)];
const CURVE_SAMPLES_PER_SEGMENT: usize = 8;

/// One cubic bezier piece of a route, in canvas-local f32 coordinates.
struct Segment {
    p0: (f32, f32),
    c1: (f32, f32),
    c2: (f32, f32),
    p1: (f32, f32),
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

fn curve_segments(route: &[Point<Pixels>], flow: Axis) -> SmallVec<[Segment; 4]> {
    route_segments(&to_f32(route), flow)
}

fn append_curve(path: &mut PathBuilder, canvas_origin: Point<Pixels>, segments: &[Segment]) {
    let (ox, oy) = (f32::from(canvas_origin.x), f32::from(canvas_origin.y));
    path.move_to(point(px(ox + segments[0].p0.0), px(oy + segments[0].p0.1)));
    for segment in segments {
        path.cubic_bezier_to(
            point(px(ox + segment.p1.0), px(oy + segment.p1.1)),
            point(px(ox + segment.c1.0), px(oy + segment.c1.1)),
            point(px(ox + segment.c2.0), px(oy + segment.c2.1)),
        );
    }
}

fn sample_curve(segments: &[Segment]) -> SmallVec<[(f32, f32); 24]> {
    let mut points = SmallVec::new();
    if let Some(first) = segments.first() {
        points.push(first.p0);
    }
    for segment in segments {
        for sample in 1..=CURVE_SAMPLES_PER_SEGMENT {
            let t = sample as f32 / CURVE_SAMPLES_PER_SEGMENT as f32;
            points.push(cubic_bezier_point(
                t, segment.p0, segment.c1, segment.c2, segment.p1,
            ));
        }
    }
    points
}

/// Expand a polyline into right-angle elbows.
///
/// Each leg turns at its midpoint on the dominant axis, giving the Z-shaped
/// run a schematic wants rather than the L an "all horizontal then all
/// vertical" rule produces — an L crowds one endpoint and reads as though the
/// line belongs to it. Legs already axis-aligned gain collinear points, which
/// paint and hit-test identically.
pub(crate) fn orthogonal_points(points: &[Point<Pixels>]) -> SmallVec<[Point<Pixels>; 12]> {
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

/// The points a shape actually draws through, in window space.
///
/// [`LineShape::Curved`] returns its control polyline; its rendered curve is
/// sampled separately, which is why hit-testing routes through
/// [`sample_line`] rather than using this directly.
fn shaped_points(
    canvas_origin: Point<Pixels>,
    points: &[Point<Pixels>],
    shape: LineShape,
) -> SmallVec<[Point<Pixels>; 12]> {
    let shifted: SmallVec<[Point<Pixels>; 12]> = points
        .iter()
        .map(|p| point(canvas_origin.x + p.x, canvas_origin.y + p.y))
        .collect();
    match shape {
        LineShape::Orthogonal => orthogonal_points(&shifted),
        _ => shifted,
    }
}

/// Paint a connector between two or more canvas-local points.
pub(crate) fn paint_line(
    canvas_origin: Point<Pixels>,
    points: &[Point<Pixels>],
    style: LineStyle,
    color: Hsla,
    window: &mut Window,
) {
    if points.len() < 2 {
        return;
    }
    if style.shape == LineShape::Curved {
        paint_curve(canvas_origin, points, style, color, window);
        return;
    }

    let drawn = shaped_points(canvas_origin, points, style.shape);
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

/// Paint a curved connector, honouring the style's dashes and width.
fn paint_curve(
    canvas_origin: Point<Pixels>,
    route: &[Point<Pixels>],
    style: LineStyle,
    color: Hsla,
    window: &mut Window,
) {
    let segments = curve_segments(route, Axis::Horizontal);
    if segments.is_empty() {
        return;
    }
    let mut path = PathBuilder::stroke(style.width);
    if style.dashed {
        path = path.dash_array(&DASH);
    }
    append_curve(&mut path, canvas_origin, &segments);
    if let Ok(p) = path.build() {
        window.paint_path(p, color);
    }
}

/// Size of an arrowhead along its axis.
const ARROW_PX: f32 = 9.0;

/// Paint a filled arrowhead at `tip`, pointing away from `from`.
///
/// A zero-length direction draws nothing rather than a degenerate spike.
pub(crate) fn paint_arrowhead(
    tip: Point<Pixels>,
    from: Point<Pixels>,
    color: Hsla,
    window: &mut Window,
) {
    let dx = f32::from(tip.x - from.x);
    let dy = f32::from(tip.y - from.y);
    let len = (dx * dx + dy * dy).sqrt();
    if !len.is_finite() || len < f32::EPSILON {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    // Perpendicular, for the two trailing corners.
    let (px_, py_) = (-uy, ux);
    let base = (
        f32::from(tip.x) - ux * ARROW_PX,
        f32::from(tip.y) - uy * ARROW_PX,
    );
    let half = ARROW_PX * 0.42;
    let mut path = PathBuilder::fill();
    path.add_polygon(
        &[
            tip,
            point(px(base.0 + px_ * half), px(base.1 + py_ * half)),
            point(px(base.0 - px_ * half), px(base.1 - py_ * half)),
        ],
        true,
    );
    if let Ok(p) = path.build() {
        window.paint_path(p, color);
    }
}

/// The connector as actually drawn, sampled densely enough that arrowheads
/// and hit-testing agree with the painted stroke. Canvas-local.
pub(crate) fn drawn_polyline(
    points: &[Point<Pixels>],
    shape: LineShape,
) -> SmallVec<[Point<Pixels>; 24]> {
    match shape {
        LineShape::Curved => sample_curve(&curve_segments(points, Axis::Horizontal))
            .into_iter()
            .map(|(x, y)| point(px(x), px(y)))
            .collect(),
        LineShape::Orthogonal => orthogonal_points(points).into_iter().collect(),
        LineShape::Straight => points.iter().copied().collect(),
    }
}

/// Distance from `pointer` to a connector, both in canvas-local coordinates.
pub(crate) fn distance_to_line(
    points: &[Point<Pixels>],
    shape: LineShape,
    pointer: Point<Pixels>,
) -> f32 {
    let samples = drawn_polyline(points, shape);
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

/// How near the pointer must be to grab a connector.
pub(crate) const LINE_HIT_RADIUS: f32 = 6.0;

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
