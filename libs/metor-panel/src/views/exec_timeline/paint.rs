//! Canvas painting for the lane area and the time ruler above it.
//!
//! Both painters run off a [`LanePaint`] snapshot taken during prepaint, so
//! nothing here reaches into an `App` — the `ClusterPaint` idiom the event-flag
//! gutter established.
//!
//! Metrics come from [`views::table`](crate::views::table): a timeline lane is
//! a table row that happens to be drawn rather than laid out, and the two must
//! line up when a timeline sits beside an outline.

use std::sync::Arc;

use gpui::{Bounds, Hsla, Pixels, Point, SharedString, Window, point, px, size};

use crate::graph_canvas::{LINE_HIT_RADIUS, LineShape, LineStyle, distance_to_line, paint_line};
use crate::theme::Theme;
use crate::views::plot_common::paint_text_label;
use crate::views::table::{HEADER_HEIGHT, ROW_HEIGHT};
use crate::views::time_series::cursor::{
    READOUT_PAD_X, READOUT_PAD_Y, READOUT_ROW_H, estimate_readout_size, readout_origin,
};
use crate::views::time_series::{
    PlotBounds, TimeFormat, format_time_label, x_tick_anchor, x_ticks,
};

use super::scan::{Bar, GanttFrame, stale_start, state_name};

/// Tallest a lane gets: one table row, so lanes and table rows line up. Below
/// this the lanes divide whatever height the pane has, and a wide topology
/// degrades to thin strips rather than scrolling out of sight.
pub(crate) const LANE_MAX: f32 = ROW_HEIGHT;
/// Height of the time ruler, which is this pane's column header.
pub(crate) const RULER_H: f32 = HEADER_HEIGHT;
/// Width of the row-label gutter — the outline's Name column at its minimum.
pub(crate) const GUTTER_W: f32 = 140.0;
/// Inset above and below a bar inside its lane, leaving the lane's own rule
/// and a little air the way a table cell's padding does.
const BAR_INSET_Y: f32 = 5.0;
/// Corner radius of a bar, matching the log panel's level pills.
const BAR_RADIUS: f32 = 3.0;
/// Border width of a bar, matching the log panel's level pills.
const BAR_BORDER: f32 = 1.0;
/// Narrowest bar that still gets pill chrome. Below it a border would be the
/// whole bar, so a sliver paints as a solid mark instead.
const BAR_PILL_MIN_W: f32 = 5.0;
/// Screen-space gap left between consecutive bars in a cycle.
///
/// The prefix sum makes bars exactly contiguous — one step ends where the next
/// begins — which leaves the connectors nowhere to land and no seam to read the
/// handover at. The gap is taken in pixels rather than in time on purpose: a
/// time-space gap would shrink a bar's apparent duration differently at every
/// zoom, which is a lie about the measurement. Half is inset from each end, and
/// only where the bar can spare it.
pub(crate) const BAR_GAP_X: f32 = 4.0;
/// Narrowest a cycle may be on screen before the data-flow connectors are
/// suppressed: below this the bars they join are not visually distinct and the
/// lines read as hatching.
const MIN_FLOW_CYCLE_PX: f32 = 24.0;
/// Ceiling on drawn connector segments per frame, so a pathological window
/// can't turn one repaint into tens of thousands of paths.
const MAX_FLOW_SEGMENTS: usize = 3_000;
/// Gap between the pointer and the hover readout.
const READOUT_OFFSET: f32 = 14.0;
/// How far outside the lane area a projected coordinate is allowed to land.
///
/// Zoom is unbounded, so a bar wider than the window projects to a rectangle
/// millions of pixels across. Nothing is gained by handing the renderer a quad
/// that size — everything past the edge is clipped anyway — and enough of them
/// have been seen to disappear entirely that it is not worth finding out where
/// the limit is. Clamping keeps every rect small, finite, and correctly ordered,
/// which is all paint needs.
const PROJECTION_MARGIN: f32 = 4096.0;

/// One data-flow connector, resolved against the *visible* lanes.
pub(crate) struct FlowPaint {
    pub from_lane: usize,
    pub to_lane: usize,
    /// A one-cycle-delayed feedback edge.
    pub dashed: bool,
    /// `"<from> → <to>"`, the readout's header.
    pub label: SharedString,
    /// `"<out> → <in>"` per grouped port pair.
    pub ports: Vec<SharedString>,
}

/// Everything one repaint of the lane area needs.
pub(crate) struct LanePaint {
    pub view: PlotBounds,
    pub frame: Option<Arc<GanttFrame>>,
    /// Indices into [`GanttFrame::bars`] for the rows actually drawn, top to
    /// bottom. Hidden rows are absent here but still present in the frame, so
    /// they keep contributing to the prefix sum.
    pub rows: Vec<usize>,
    /// Row labels, parallel to `rows`, for the hover readout.
    pub labels: Vec<SharedString>,
    pub flows: Vec<FlowPaint>,
    pub hover: Option<Point<Pixels>>,
    pub time_format: TimeFormat,
    pub data_start: f64,
    pub theme: Arc<Theme>,
}

/// Height of one lane given the area and how many rows share it.
pub(crate) fn lane_height(area_height: Pixels, rows: usize) -> Pixels {
    if rows == 0 {
        return px(LANE_MAX);
    }
    px((f32::from(area_height) / rows as f32).min(LANE_MAX))
}

/// The time-to-pixel mapping for one repaint, and its inverse.
///
/// Built once per painter rather than derived at each call site, because the
/// arithmetic has a trap in it. Timestamps here are epoch microseconds — around
/// 1.7e15 — while `Pixels` is `f32`, whose 24-bit mantissa resolves that
/// magnitude only to about a minute. Any value that reaches `f32` still carrying
/// the epoch is destroyed, and the deeper the zoom the larger the scale that
/// multiplies the error. So the rebase against the window origin happens in
/// `i64`, the scale in `f64`, and only the final small pixel offset narrows —
/// the same epoch-first discipline the plot's GPU planner applies before it
/// hands vertices to the shader.
#[derive(Clone, Copy)]
pub(crate) struct Projection {
    /// Window start, the epoch every timestamp is measured against.
    origin_us: i64,
    px_per_us: f64,
    left: f32,
    right: f32,
}

impl Projection {
    pub(crate) fn new(view: &PlotBounds, bounds: Bounds<Pixels>) -> Self {
        let left = f32::from(bounds.origin.x);
        Self {
            origin_us: view.min_x as i64,
            px_per_us: f64::from(f32::from(bounds.size.width)) / view.width().max(1.0),
            left,
            right: left + f32::from(bounds.size.width),
        }
    }

    /// Where `t_us` lands, clamped to [`PROJECTION_MARGIN`] beyond each edge.
    ///
    /// The clamp is order-preserving, so a bar that starts before the window and
    /// ends after it still yields a rectangle covering the window; one that lies
    /// wholly outside collapses against a single edge, where the caller's
    /// existing cull drops it.
    pub(crate) fn x(&self, t_us: i64) -> Pixels {
        let offset = (t_us - self.origin_us) as f64 * self.px_per_us;
        let x = (self.left as f64 + offset) as f32;
        px(x.clamp(
            self.left - PROJECTION_MARGIN,
            self.right + PROJECTION_MARGIN,
        ))
    }

    /// The timestamp under a screen x. Rebased the same way, so the hover
    /// readout keeps working at any zoom.
    pub(crate) fn time_at(&self, x: Pixels) -> i64 {
        if self.px_per_us <= 0.0 {
            return self.origin_us;
        }
        let offset = (f32::from(x) - self.left) as f64 / self.px_per_us;
        self.origin_us.saturating_add(offset as i64)
    }
}

/// What the ruler needs: the window and how to label it.
pub(crate) struct RulerPaint {
    pub view: PlotBounds,
    pub time_format: TimeFormat,
    pub data_start: f64,
    pub theme: Arc<Theme>,
}

/// The time ruler, styled as the table's column header: same height, same
/// bottom rule, same cell font.
pub(crate) fn paint_ruler(
    bounds: Bounds<Pixels>,
    paint: &RulerPaint,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let view = &paint.view;
    let theme = &paint.theme;
    let fmt = paint.time_format;
    let span = view.width();
    let ref_us = paint.data_start as i64;
    let proj = Projection::new(view, bounds);
    let anchor = x_tick_anchor(fmt, paint.data_start);
    for tick in x_ticks(view, 6, anchor) {
        let x = proj.x(tick);
        window.paint_quad(gpui::fill(
            Bounds::new(point(x, bounds.origin.y), size(px(1.0), bounds.size.height)),
            theme.grid_color,
        ));
        let label = format_time_label(tick, ref_us, fmt, span);
        paint_text_label(
            label,
            theme.text_tertiary,
            |w, h| {
                point(
                    (x + px(3.0)).min(bounds.origin.x + bounds.size.width - w),
                    bounds.origin.y + (bounds.size.height - h) / 2.0,
                )
            },
            window,
            cx,
        );
    }
}

/// The lanes: the encompassing whole-cycle context band, each row's bars, the
/// data-flow connectors, the stale tail, and the hover readout.
pub(crate) fn paint_lanes(
    bounds: Bounds<Pixels>,
    paint: &LanePaint,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let theme = &paint.theme;
    let view = &paint.view;
    let proj = Projection::new(view, bounds);
    let lane_h = lane_height(bounds.size.height, paint.rows.len());
    let right = bounds.origin.x + bounds.size.width;

    // Lane rules, matching the table's per-row bottom border.
    for i in 0..paint.rows.len() {
        let top = bounds.origin.y + lane_h * i as f32;
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(bounds.origin.x, top + lane_h - px(1.0)),
                size(bounds.size.width, px(1.0)),
            ),
            theme.border_primary,
        ));
    }

    let Some(frame) = paint.frame.as_ref() else {
        return;
    };

    // The whole-cycle band sits behind every lane: the gap between it and the
    // sum of the bars in front of it is the host's own overhead. It stays even
    // though the envelope now has its own lane at the top — the lane says how
    // long the cycle was, the band says which bars belong to it, and following
    // one bar down to its cycle is the reading the band exists for.
    for (ts, dur) in &frame.cycle {
        let x0 = proj.x(*ts);
        let x1 = proj.x(ts.saturating_add(*dur as i64));
        if x1 < bounds.origin.x || x0 > right {
            continue;
        }
        let w = (x1 - x0).max(px(1.0));
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(x0, bounds.origin.y),
                size(w, lane_h * paint.rows.len() as f32),
            ),
            Theme::dim(theme.text_tertiary, 0.07),
        ));
    }

    for (lane, row) in paint.rows.iter().enumerate() {
        let Some(bars) = frame.bars.get(*row) else {
            continue;
        };
        // The envelope keeps the lanes' geometry and differs only in colour;
        // it reads as the container through its neutral tint and its width,
        // not by breaking the row rhythm.
        let envelope = *row == 0;
        for bar in bars {
            let Some(rect) = bar_rect(bar, frame, &proj, bounds, lane, lane_h) else {
                continue;
            };
            if rect.origin.x > right {
                break;
            }
            paint_bar(rect, bar, frame, envelope, theme, window);
        }
    }

    let lines = flow_lines(paint, frame, &proj, bounds, lane_h);
    let flow_color = theme.frame_edge_color();
    for line in &lines {
        paint_line(
            point(px(0.0), px(0.0)),
            &line.points,
            LineStyle {
                width: px(1.0),
                dashed: line.flow.dashed,
                shape: LineShape::Orthogonal,
            },
            Theme::dim(flow_color, 0.7),
            window,
        );
    }

    // A target that stopped reporting must not read as still running. The cue
    // opens one period past the newest record, so a window sitting on history
    // or triggered onto the newest cycle is never dimmed — only genuinely
    // unreported time is.
    if let Some(edge) = stale_start(frame.data_end, frame.period_us, view.min_x, view.max_x) {
        let x = proj.x(edge as i64).max(bounds.origin.x);
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(x, bounds.origin.y),
                size(right - x, bounds.size.height),
            ),
            Theme::dim(theme.bg_secondary, 0.55),
        ));
        window.paint_quad(gpui::fill(
            Bounds::new(point(x, bounds.origin.y), size(px(1.0), bounds.size.height)),
            Theme::dim(theme.text_tertiary, 0.5),
        ));
    }

    paint_hover(bounds, paint, frame, lane_h, &lines, window, cx);
}

/// The horizontal extent a bar is actually painted across.
///
/// The single source of truth for a bar's left and right edges, so the
/// connectors anchor exactly where the pills end — a line that stopped at the
/// true extent would disappear under the neighbouring bar it is supposed to
/// point at. [`BAR_GAP_X`] is split between the two ends and skipped entirely
/// on a bar too narrow to give it up, which is what keeps a sub-pixel step
/// visible instead of insetting it out of existence.
pub(crate) fn bar_span(bar: &Bar, frame: &GanttFrame, proj: &Projection) -> (Pixels, Pixels) {
    let left = proj.x(bar.start_us);
    let end = if frame.summarized {
        bar.start_us.saturating_add(frame.bucket_us.max(1))
    } else {
        bar.start_us.saturating_add(bar.dur_us as i64)
    };
    let right = proj.x(end).max(left + px(1.0));
    let inset = if right - left >= px(BAR_GAP_X + BAR_PILL_MIN_W) {
        px(BAR_GAP_X / 2.0)
    } else {
        px(0.0)
    };
    (left + inset, right - inset)
}

/// Screen rectangle of one bar, or `None` when it falls outside the view.
pub(crate) fn bar_rect(
    bar: &Bar,
    frame: &GanttFrame,
    proj: &Projection,
    bounds: Bounds<Pixels>,
    lane: usize,
    lane_h: Pixels,
) -> Option<Bounds<Pixels>> {
    let (left, right) = bar_span(bar, frame, proj);
    if right < bounds.origin.x {
        return None;
    }
    let inset = px(BAR_INSET_Y).min(lane_h / 3.0);
    let top = bounds.origin.y + lane_h * lane as f32 + inset;
    let h = (lane_h - inset * 2.0 - px(1.0)).max(px(2.0));
    Some(Bounds::new(point(left, top), size(right - left, h)))
}

/// Draw one bar as a level pill: tinted fill inside a bright border in the
/// state's colour, at the log panel's radius. A bar too narrow to hold a
/// border paints as a solid sliver instead — pill chrome on a 2 px mark is
/// just a smear.
///
/// The envelope's bar takes the same pill geometry as the lanes below it and
/// differs only in colour — neutral, because it has no run state of its own.
/// Its span across the whole cycle is what identifies it; breaking the shared
/// shape as well only made the row look like a different kind of object.
fn paint_bar(
    rect: Bounds<Pixels>,
    bar: &Bar,
    frame: &GanttFrame,
    envelope: bool,
    theme: &Theme,
    window: &mut Window,
) {
    let code = bar.state as usize;
    // A summary bucket's busy fraction rides the alpha, so a lightly loaded
    // stretch reads as faint without changing its colour.
    let alpha = if frame.summarized {
        (bar.dur_us as f64 / frame.bucket_us.max(1) as f64).clamp(0.15, 1.0) as f32
    } else {
        1.0
    };
    let border = if envelope {
        Theme::dim(theme.text_secondary, alpha)
    } else {
        Theme::dim(theme.slot_state_color(code), alpha)
    };
    if f32::from(rect.size.width) < BAR_PILL_MIN_W {
        window.paint_quad(gpui::fill(rect, border));
        return;
    }
    let mut fill = if envelope {
        Theme::dim(theme.text_secondary, 0.10)
    } else {
        theme.slot_state_tint(code)
    };
    fill.a *= alpha;
    window.paint_quad(gpui::quad(
        rect,
        px(BAR_RADIUS),
        fill,
        px(BAR_BORDER),
        border,
        gpui::BorderStyle::Solid,
    ));
}

/// One drawn connector instance: a producer bar's right edge to its consumer's
/// left edge.
struct FlowLine<'a> {
    flow: &'a FlowPaint,
    points: [Point<Pixels>; 2],
}

/// The connectors to draw this frame.
///
/// Suppressed wholesale in summarized mode (there are no per-cycle bars to
/// join) and whenever a cycle is narrower than [`MIN_FLOW_CYCLE_PX`], where the
/// lines would out-ink the bars they annotate. Cycle width comes from the
/// frame's measured period rather than from counting records inside the window,
/// so the test still works when the window is *narrower* than one cycle — which
/// is exactly when the lines are most worth drawing.
fn flow_lines<'a>(
    paint: &'a LanePaint,
    frame: &GanttFrame,
    proj: &Projection,
    bounds: Bounds<Pixels>,
    lane_h: Pixels,
) -> Vec<FlowLine<'a>> {
    let mut out = Vec::new();
    if frame.summarized || paint.flows.is_empty() || frame.period_us <= 0 {
        return out;
    }
    let view = &paint.view;
    let cycle_px =
        f32::from(bounds.size.width) * (frame.period_us as f64 / view.width().max(1.0)) as f32;
    if cycle_px <= MIN_FLOW_CYCLE_PX {
        return out;
    }

    let center = |lane: usize| bounds.origin.y + lane_h * lane as f32 + lane_h / 2.0;
    for flow in &paint.flows {
        let (Some(&from_row), Some(&to_row)) =
            (paint.rows.get(flow.from_lane), paint.rows.get(flow.to_lane))
        else {
            continue;
        };
        let (Some(from_bars), Some(to_bars)) = (frame.bars.get(from_row), frame.bars.get(to_row))
        else {
            continue;
        };
        for consumer in to_bars {
            let Some(producer) = producer_bar(from_bars, consumer.cycle_ts, flow.dashed) else {
                continue;
            };
            // Anchored on the painted edges, not the true extents, so the line
            // starts and ends in the gap between the pills rather than beneath
            // them.
            let start = bar_span(producer, frame, proj).1;
            let end = bar_span(consumer, frame, proj).0;
            if start > bounds.origin.x + bounds.size.width || end < bounds.origin.x {
                continue;
            }
            out.push(FlowLine {
                flow,
                points: [
                    point(start, center(flow.from_lane)),
                    point(end, center(flow.to_lane)),
                ],
            });
            if out.len() >= MAX_FLOW_SEGMENTS {
                return out;
            }
        }
    }
    out
}

/// The producer bar a consumer's `cycle` reads from: the one in the same cycle
/// for a forward edge, the newest one strictly before it for a delayed edge.
///
/// `bars` is sorted, so both are one binary search. A delayed edge whose
/// previous cycle fell outside the read window simply has no producer and is
/// dropped — better a missing line than one anchored at the wrong cycle.
fn producer_bar(bars: &[Bar], cycle: i64, delayed: bool) -> Option<&Bar> {
    let at = bars.partition_point(|b| b.cycle_ts < cycle);
    if delayed {
        at.checked_sub(1).and_then(|i| bars.get(i))
    } else {
        bars.get(at).filter(|b| b.cycle_ts == cycle)
    }
}

/// The readout under the pointer.
///
/// A connector wins over a bar when the pointer is near one: the lines sit on
/// top, and a line is the harder target of the two.
fn paint_hover(
    bounds: Bounds<Pixels>,
    paint: &LanePaint,
    frame: &GanttFrame,
    lane_h: Pixels,
    lines: &[FlowLine<'_>],
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let Some(pos) = paint.hover else {
        return;
    };
    // Rebuilt rather than threaded through: it is a pure function of the same
    // view and bounds the caller used, so the two cannot drift.
    let proj = Projection::new(&paint.view, bounds);
    if !bounds.contains(&pos) || lane_h <= px(0.0) {
        return;
    }
    if let Some(flow) = nearest_flow(lines, pos) {
        paint_readout(
            bounds,
            pos,
            flow.label.clone(),
            &flow.ports,
            &paint.theme,
            window,
            cx,
        );
        return;
    }

    let lane = (f32::from(pos.y - bounds.origin.y) / f32::from(lane_h)) as usize;
    let (Some(row), Some(label)) = (paint.rows.get(lane), paint.labels.get(lane)) else {
        return;
    };
    let Some(bars) = frame.bars.get(*row) else {
        return;
    };
    // The inverse mapping rebases like the forward one; deriving the time from
    // a window fraction in f32 would land minutes away at epoch scale.
    let t = proj.time_at(pos.x);
    let at = bars.partition_point(|b| b.start_us <= t);
    let Some(bar) = at.checked_sub(1).and_then(|i| bars.get(i)) else {
        return;
    };
    let span = if frame.summarized {
        frame.bucket_us.max(1)
    } else {
        bar.dur_us.max(1) as i64
    };
    if t > bar.start_us.saturating_add(span) {
        return;
    }

    let cycle = format_time_label(
        bar.cycle_ts,
        paint.data_start as i64,
        paint.time_format,
        paint.view.width(),
    );
    let busy = hifitime::Duration::from_microseconds(bar.dur_us as f64);
    let detail = [
        SharedString::from(format!("cycle {cycle}")),
        SharedString::from(format!("took {busy}")),
        SharedString::from(state_name(bar.state)),
    ];
    paint_readout(
        bounds,
        pos,
        label.clone(),
        &detail,
        &paint.theme,
        window,
        cx,
    );
}

/// The connector within [`LINE_HIT_RADIUS`] of the pointer, nearest first.
fn nearest_flow<'a>(lines: &'a [FlowLine<'a>], pointer: Point<Pixels>) -> Option<&'a FlowPaint> {
    let mut best: Option<(f32, &FlowPaint)> = None;
    for line in lines {
        let d = distance_to_line(&line.points, LineShape::Orthogonal, pointer);
        if d <= LINE_HIT_RADIUS && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
            best = Some((d, line.flow));
        }
    }
    best.map(|(_, flow)| flow)
}

/// Paint a header plus detail lines in a box placed clear of the pointer, the
/// same geometry the plots' hover readout uses.
fn paint_readout(
    bounds: Bounds<Pixels>,
    pointer: Point<Pixels>,
    header: SharedString,
    detail: &[SharedString],
    theme: &Theme,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let lens: Vec<(usize, usize)> = detail.iter().map(|d| (d.chars().count(), 0)).collect();
    let box_size = estimate_readout_size(header.chars().count(), &lens);
    let origin = readout_origin(pointer, box_size, bounds, px(READOUT_OFFSET));
    window.paint_quad(gpui::quad(
        Bounds::new(origin, box_size),
        px(BAR_RADIUS),
        theme.bg_elevated,
        px(BAR_BORDER),
        theme.border_primary,
        gpui::BorderStyle::Solid,
    ));
    let mut y = origin.y + px(READOUT_PAD_Y);
    let mut line = |text: SharedString, color: Hsla, window: &mut Window, cx: &mut gpui::App| {
        paint_text_label(
            text,
            color,
            |_, _| point(origin.x + px(READOUT_PAD_X), y),
            window,
            cx,
        );
        y += px(READOUT_ROW_H);
    };
    line(header, theme.text_primary, window, cx);
    for text in detail {
        line(text.clone(), theme.text_secondary, window, cx);
    }
}
