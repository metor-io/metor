use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, Bounds, Context, Entity, Hsla, IntoElement, MouseButton, PathBuilder, Pixels,
    Point, SharedString, Styled, Subscription, Window, canvas, div, point, prelude::*, px,
};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, PrimType, Timestamp};

#[allow(unused_imports)]
use crate::inspect;
use crate::plot_events::{EventDetail, EventKindKey, EventSource, EventSourceRegistry, PlotEvent};
use crate::views::json_tree::JsonTree;

mod axis;
pub use axis::YAxis;

mod config;
pub use config::{
    EventOverlayConfig, MeasurementCursorConfig, MeasurementPanelConfig, PlotPanelConfig,
    TraceConfig, YAxisConfig,
};

mod event_overlay;
pub use event_overlay::EventOverlay;

mod event_flags;
use event_flags::{
    ClusterPaint, EventCluster, FLAG_HIT_PX, GUTTER_H, cluster_events, paint_event_flags,
};

mod bounds;
pub use bounds::*;

mod gpu;
pub(crate) use gpu::{AxisSource, LineDraw, PlotRenderState};

mod line_plot;
pub use line_plot::LinePlot;

mod override_field;
pub use override_field::Override;

pub mod time_range;
pub use time_range::TimeRangeBehavior;

pub mod cursor;
pub use cursor::{CursorId, MeasurementCursor};

pub mod measurements;
pub use measurements::{MeasurementKind, MeasurementKindList};

pub mod cursor_inspector;
pub use cursor_inspector::build_cursor_rows;

pub mod measurement_panel;
pub use measurement_panel::PanelPosition;

/// Iterator of tick values for any value axis on a `[min, max)` range.
///
/// Ticks are anchored at zero whenever the range crosses it so the origin
/// stays on a labeled line, and are generated as `start + i * step` rather
/// than by accumulation so float drift can't drop the top tick. Near-zero
/// values are snapped to exact zero to avoid the `-5.6e-17` artifacts that
/// show up with floating arithmetic. Used for both Y axes on time-series
/// plots and both axes on XY plots.
pub(crate) fn value_ticks(min: f64, max: f64, target_count: usize) -> impl Iterator<Item = f64> {
    let step = pretty_round((max - min) / target_count as f64);
    let (start, count) = if !step.is_normal() || step <= 0.0 {
        (0.0, 0)
    } else {
        let (start, end) = if min <= 0.0 && max >= 0.0 {
            // Anchor at 0: find the lowest tick by stepping down from 0
            let neg_steps = (-min / step).floor() as i64;
            (-step * neg_steps as f64, max)
        } else {
            ((min / step).ceil() * step, max + step * 0.01)
        };
        // The epsilon rescues a top tick whose quotient lands barely under
        // an integer through division rounding alone.
        let steps = ((end - start) / step + 1e-9).floor();
        if steps < 0.0 {
            (start, 0)
        } else {
            (start, steps as usize + 1)
        }
    };
    (0..count).map(move |i| {
        let v = start + i as f64 * step;
        if v.abs() < step * 1e-10 { 0.0 } else { v }
    })
}

/// Round a duration up to the next "nice" tick size (1/5/15/30 sec, 1 min,
/// 1 hour, 1 day, …) so x-axis labels land on human-friendly intervals.
#[allow(clippy::match_overlapping_arm)]
fn segment_round(dur: hifitime::Duration) -> hifitime::Duration {
    use hifitime::{TimeUnits, Unit};
    let (_, days, hours, minutes, seconds, milli, us, _) = dur.decompose();
    let round_to = if days > 0 {
        match days {
            ..=2 => 1,
            ..=5 => 5,
            ..=15 => 15,
            ..=30 => 30,
            _ => 50,
        }
        .days()
    } else if hours > 0 {
        match hours {
            ..=2 => 1,
            ..=6 => 6,
            ..=12 => 12,
            _ => 24,
        }
        .hours()
    } else if minutes > 0 {
        match minutes {
            ..=2 => 1,
            ..=5 => 5,
            ..=15 => 15,
            ..=30 => 30,
            _ => 60,
        }
        .minutes()
    } else if seconds > 0 {
        match seconds {
            ..=2 => 1,
            ..=5 => 5,
            ..=15 => 15,
            ..=30 => 30,
            _ => 60,
        }
        .seconds()
    } else if milli > 0 {
        match milli {
            ..=2 => 1,
            ..=5 => 5,
            ..=10 => 10,
            ..=25 => 25,
            ..=50 => 50,
            ..=100 => 100,
            ..=250 => 250,
            ..=500 => 500,
            _ => 1000,
        }
        .milliseconds()
    } else if us > 0 {
        1 * Unit::Microsecond
    } else {
        1 * Unit::Nanosecond
    };

    dur.ceil(round_to)
}

/// Iterator of X-axis tick values (microseconds) aligned to
/// human-readable offsets from `anchor`.
///
/// `anchor` is the earliest sample for relative labels (so ticks don't
/// crawl during pan) and Unix epoch `0` for absolute labels (so ticks land
/// on wall-clock boundaries like `:00`/`:30`).
fn x_ticks(view: &PlotBounds, target_count: usize, anchor: f64) -> impl Iterator<Item = i64> {
    // Backstop: `segment_round` keeps the count near `target_count`, but a
    // paint pass must never be able to loop unbounded if it degenerates.
    const MAX_TICKS: usize = 1024;
    let range = view.width();
    let target = (target_count as f64).max(1.0);
    let raw_step_us = range / target;
    let step_dur = segment_round(hifitime::Duration::from_microseconds(raw_step_us));
    // Sub-microsecond windows round to a zero-microsecond step; clamp to
    // one so the iterator always advances.
    let step_i = ((step_dur.total_nanoseconds() / 1_000) as i64).max(1);

    let t_min = view.min_x as i64;
    let t_max = view.max_x as i64;
    let ds = anchor as i64;

    // Ticks are snapped to multiples of `step_i` measured from the anchor
    // so scrubbing doesn't make labels crawl across the axis.
    let aligned = ds + ((t_min - ds) / step_i) * step_i;
    let first = if aligned < t_min {
        aligned + step_i
    } else {
        aligned
    };
    // A degenerate window still labels its position: yield `t_min` once.
    let (start, end) = if range > 0.0 {
        (first, t_max)
    } else {
        (t_min, t_min)
    };

    let mut t = start;
    let mut yielded = 0;
    std::iter::from_fn(move || {
        if t > end || yielded >= MAX_TICKS {
            return None;
        }
        let tick = t;
        t = t.saturating_add(step_i);
        yielded += 1;
        Some(tick)
    })
}

/// Anchor passed to [`x_ticks`]: the earliest sample for relative labels,
/// Unix epoch `0` for absolute ones (so ticks land on wall-clock boundaries).
fn x_tick_anchor(fmt: TimeFormat, data_start: f64) -> f64 {
    match fmt {
        TimeFormat::Relative => data_start,
        TimeFormat::Utc | TimeFormat::Local => 0.0,
    }
}

/// Render one X-axis tick label.
///
/// `Relative` shows the offset from `ref_us` (`"0"`, `"1 min 30 s"`).
/// `Utc`/`Local` render absolute wall-clock time; `span_us` (the visible
/// range) picks the granularity — date is folded in once the window spans
/// more than a day. Falls back to the relative label if the timestamp is
/// outside jiff's representable range.
fn format_time_label(t_us: i64, ref_us: i64, fmt: TimeFormat, span_us: f64) -> String {
    let relative = || {
        let offset_us = t_us - ref_us;
        if offset_us == 0 {
            "0".to_string()
        } else {
            format!(
                "{}",
                hifitime::Duration::from_microseconds(offset_us as f64)
            )
        }
    };
    match fmt {
        TimeFormat::Relative => relative(),
        TimeFormat::Utc | TimeFormat::Local => {
            let Ok(ts) = jiff::Timestamp::from_microsecond(t_us) else {
                return relative();
            };
            let pattern = if span_us > 86_400.0 * 1e6 {
                "%m-%d %H:%M"
            } else {
                "%H:%M:%S"
            };
            match fmt {
                TimeFormat::Local => ts
                    .to_zoned(jiff::tz::TimeZone::system())
                    .strftime(pattern)
                    .to_string(),
                _ => ts.strftime(pattern).to_string(),
            }
        }
    }
}

pub(crate) fn format_value_label(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    let abs = v.abs();
    if abs >= 1000.0 {
        format!("{:.0}", v)
    } else if abs >= 1.0 {
        format!("{:.1}", v)
    } else if abs >= 0.01 {
        format!("{:.2}", v)
    } else {
        format!("{:.1e}", v)
    }
}

pub(crate) const Y_LABEL_WIDTH: f32 = 50.0;
pub(crate) const X_LABEL_HEIGHT: f32 = 10.0;
pub(crate) const PADDING: f32 = 8.0;
pub(crate) const LABEL_FONT_SIZE: f32 = 11.0;

/// Gap between the pointer and the hover readout box so the box never sits
/// under the cursor.
const HOVER_READOUT_OFFSET: f32 = 14.0;
/// Inner padding on the hover readout box; mirrors the estimate in
/// [`cursor::estimate_readout_size`] so placement matches the painted box.
const READOUT_PAD_X: f32 = 6.0;
const READOUT_PAD_Y: f32 = 4.0;

/// One readout line: the trace's color, its label, and the formatted value
/// at the crosshair timestamp.
type HoverRows = smallvec::SmallVec<[(Hsla, SharedString, SharedString); 4]>;

/// One visible trace's nearest sample to the hover crosshair — everything
/// the readout rows and the on-plot sample markers need, produced by the
/// single lookup in [`TimeSeriesPlot::hover_samples`].
struct HoverSample {
    color: Hsla,
    label: SharedString,
    /// The sample's actual timestamp (not the pointer's), so the marker
    /// lands on the data point rather than on the crosshair.
    ts: Timestamp,
    value: f64,
    axis_index: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AxisZone {
    Plot,
    XAxis,
    /// Pointer is over the Y-axis column for this axis index. Columns stack
    /// outward to the left: index 0 sits adjacent to the plot, higher indices
    /// further left.
    YAxis(usize),
}

/// Total left chrome width for `axis_count` stacked Y-axis columns plus the
/// inner padding. Drives both [`plot_area`] and the overlaid child insets.
pub(crate) fn left_margin(axis_count: usize) -> f32 {
    axis_count.max(1) as f32 * Y_LABEL_WIDTH + PADDING
}

pub(crate) fn axis_zone(
    pos: Point<Pixels>,
    plot_area: Bounds<Pixels>,
    axis_count: usize,
) -> AxisZone {
    let below_plot = pos.y > plot_area.origin.y + plot_area.size.height;
    let left_of_plot = pos.x < plot_area.origin.x;
    if left_of_plot {
        // Columns stack outward: index = how many Y_LABEL_WIDTH steps left of
        // the plot edge the pointer sits.
        let dist = f32::from(plot_area.origin.x - pos.x).max(0.0);
        let idx = (dist / Y_LABEL_WIDTH) as usize;
        AxisZone::YAxis(idx.min(axis_count.saturating_sub(1)))
    } else if below_plot {
        AxisZone::XAxis
    } else {
        AxisZone::Plot
    }
}

pub(crate) fn plot_area(outer: Bounds<Pixels>, axis_count: usize) -> Bounds<Pixels> {
    let left = left_margin(axis_count);
    Bounds {
        origin: point(outer.origin.x + px(left), outer.origin.y + px(PADDING)),
        size: gpui::Size {
            width: (outer.size.width - px(left + PADDING)).max(px(1.0)),
            height: (outer.size.height - px(X_LABEL_HEIGHT + PADDING * 2.0)).max(px(1.0)),
        },
    }
}

/// Cached `(min, max)` over the first `sample_count` samples of one
/// [`TimeSeriesNode`](metor_db::TimeSeriesNode).
///
/// Sealed nodes are immutable so their entry is stable for the rest of the
/// plot's lifetime; only the head node's entry extends.
#[derive(Clone, Copy)]
pub struct NodeBounds {
    pub sample_count: usize,
    pub min: f64,
    pub max: f64,
}

/// Per-trace cache keyed by a node's first sample timestamp — the node's
/// identity everywhere else in the DB (directory name, manifest span key,
/// `begin_fetch`), unique per component by construction.
///
/// Not keyed by `Arc::as_ptr`: splices free node shells and reallocate
/// same-sized ones, so a pointer can collide across frames and one node
/// would inherit another's cached min/max. The first timestamp survives
/// shell rebuilds (the rebuilt shell wraps the same inner value). Entries
/// for nodes that leave the component's live set are evicted on each call.
pub type NodeBoundsCache = HashMap<i64, NodeBounds>;

/// Aggregate `(min, max)` across `indexes` over every sample in
/// `component.time_series`, reusing `cache` so only new head-node samples
/// are scanned.
///
/// Used for both Y-axis bound tracking on time-series plots and per-axis
/// bound tracking on XY plots (called twice — once per axis component).
///
/// The inner loop dispatches on `PrimType` once per node and reads values
/// straight from the memory-mapped byte buffer, avoiding any per-sample
/// `ComponentView`, `ElementValue`, or `Result` allocation in the hot path.
pub fn expand_value_bounds(
    component: &Component,
    indexes: &[usize],
    cache: &mut NodeBoundsCache,
) -> Option<(f64, f64)> {
    let schema = &component.schema;
    let sample_size = schema.size();
    if sample_size == 0 || indexes.is_empty() {
        return None;
    }
    let prim_size = schema.prim_type.size();
    let mut offsets: smallvec::SmallVec<[usize; 4]> = smallvec::SmallVec::new();
    for &idx in indexes {
        let off = idx * prim_size;
        if off + prim_size <= sample_size {
            offsets.push(off);
        }
    }
    if offsets.is_empty() {
        return None;
    }

    let mut agg_min = f64::INFINITY;
    let mut agg_max = f64::NEG_INFINITY;
    let mut seen = false;
    let mut live: HashSet<i64> = HashSet::new();

    for node in component.time_series.list.iter() {
        // First timestamp is the node's stable identity; an empty node
        // caches nothing, so skip it (it can't collide a key either).
        let Some(node_id) = node.timestamps().first().map(|t| t.0) else {
            continue;
        };
        live.insert(node_id);
        let current_len = node.timestamps().len();
        let entry = cache.entry(node_id).or_insert(NodeBounds {
            sample_count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        });
        if current_len > entry.sample_count {
            let data = node.data.data();
            let from = entry.sample_count * sample_size;
            let to = current_len * sample_size;
            if to <= data.len() {
                let (min, max) =
                    scan_min_max_dispatch(schema.prim_type, &data[from..to], sample_size, &offsets);
                if min < entry.min {
                    entry.min = min;
                }
                if max > entry.max {
                    entry.max = max;
                }
                entry.sample_count = current_len;
            }
        }
        if entry.sample_count > 0 && entry.min.is_finite() {
            if entry.min < agg_min {
                agg_min = entry.min;
            }
            if entry.max > agg_max {
                agg_max = entry.max;
            }
            seen = true;
        }
    }

    cache.retain(|id, _| live.contains(id));
    seen.then_some((agg_min, agg_max))
}

/// `(min, max)` across all elements of the latest sample, treating the
/// sample as a flat vector of length `len`.
///
/// Differs from [`expand_value_bounds`]: that scans across many samples
/// for one element index; this scans across many element indices for one
/// sample. Used by list plots, which redraw fully whenever a new sample
/// arrives — there is no cross-tick caching opportunity.
pub fn expand_latest_sample_bounds(component: &Component, len: usize) -> Option<(f64, f64)> {
    if len == 0 {
        return None;
    }
    let schema = &component.schema;
    let prim_size = schema.prim_type.size();
    if prim_size == 0 {
        return None;
    }
    let latest = component.time_series.latest()?;
    let sample = latest.data();
    if sample.len() < len * prim_size {
        return None;
    }
    let mut offsets: smallvec::SmallVec<[usize; 16]> = smallvec::SmallVec::with_capacity(len);
    for i in 0..len {
        offsets.push(i * prim_size);
    }
    // `scan_min_max` chunks `data` by `sample_size` — pass a slice of one
    // sample so it iterates exactly once and inspects every offset.
    let (min, max) = scan_min_max_dispatch(
        schema.prim_type,
        &sample[..len * prim_size],
        len * prim_size,
        &offsets,
    );
    if min.is_finite() && max.is_finite() {
        Some((min, max))
    } else {
        None
    }
}

fn scan_min_max_dispatch(
    prim: PrimType,
    data: &[u8],
    sample_size: usize,
    offsets: &[usize],
) -> (f64, f64) {
    match prim {
        // Bool is one byte; treat as u8 (0/1) for min/max.
        PrimType::U8 | PrimType::Bool => scan_min_max::<u8>(data, sample_size, offsets),
        PrimType::U16 => scan_min_max::<u16>(data, sample_size, offsets),
        PrimType::U32 => scan_min_max::<u32>(data, sample_size, offsets),
        PrimType::U64 => scan_min_max::<u64>(data, sample_size, offsets),
        PrimType::I8 => scan_min_max::<i8>(data, sample_size, offsets),
        PrimType::I16 => scan_min_max::<i16>(data, sample_size, offsets),
        PrimType::I32 => scan_min_max::<i32>(data, sample_size, offsets),
        PrimType::I64 => scan_min_max::<i64>(data, sample_size, offsets),
        PrimType::F32 => scan_min_max::<f32>(data, sample_size, offsets),
        PrimType::F64 => scan_min_max::<f64>(data, sample_size, offsets),
    }
}

#[inline]
fn scan_min_max<T: gpu::PlotValue>(
    data: &[u8],
    sample_size: usize,
    offsets: &[usize],
) -> (f64, f64) {
    let t_size = std::mem::size_of::<T>();
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for sample in data.chunks_exact(sample_size) {
        for &off in offsets {
            let Some(buf) = sample.get(off..off + t_size) else {
                continue;
            };
            let Ok(v) = T::read_from_bytes(buf) else {
                continue;
            };
            let f = v.to_f64();
            if f < min {
                min = f;
            }
            if f > max {
                max = f;
            }
        }
    }
    (min, max)
}

/// Paint gridlines and the zero line behind the GPU-rendered line plot.
///
/// Called before [`LinePlot`] paints so the grid sits under the series.
/// Highest-severity active-alarm tint across the plot's visible traces, if any.
/// `None` when no alarm is active or the alarm store isn't initialized.
fn alarm_plot_tint(lp: &LinePlot, cx: &gpui::App) -> Option<Hsla> {
    if !lp.show_alarm_color {
        return None;
    }
    let store = crate::alarms::try_global(cx)?;
    let store = store.read(cx);
    let state = store.state();
    let mut worst: Option<usize> = None;
    for trace in lp.traces() {
        let cfg = trace.read(cx);
        if !cfg.visible {
            continue;
        }
        if let Some(severity) = state.active_severity_for(cfg.component_id, cfg.element_index) {
            let idx = crate::alarms::severity_index(severity);
            worst = Some(worst.map_or(idx, |w| w.max(idx)));
        }
    }
    worst.map(|idx| crate::theme::theme(cx).alarm_tint(idx))
}

/// Display limit lines `(axis_index, value, color, label)` declared for the plot's
/// visible traces by the control system's alarm definitions.
fn alarm_limit_lines(lp: &LinePlot, cx: &gpui::App) -> Vec<(usize, f64, Hsla, SharedString)> {
    if !lp.show_alarm_limits {
        return Vec::new();
    }
    let Some(store) = crate::alarms::try_global(cx) else {
        return Vec::new();
    };
    let store = store.read(cx);
    let state = store.state();
    let theme = crate::theme::theme(cx);
    let mut lines = Vec::new();
    for trace in lp.traces() {
        let cfg = trace.read(cx);
        if !cfg.visible {
            continue;
        }
        for limit in state.limits_for(cfg.component_id, cfg.element_index) {
            let idx = crate::alarms::severity_index(limit.severity);
            let label = limit.label.map(SharedString::from).unwrap_or_default();
            lines.push((cfg.axis_index, limit.value, theme.alarm_color(idx), label));
        }
    }
    lines
}

fn paint_underlay(
    outer_bounds: Bounds<Pixels>,
    view: &PlotView,
    data_start: f64,
    fmt: TimeFormat,
    tint: Option<Hsla>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let pb = plot_area(outer_bounds, view.axis_count());
    let theme = crate::theme::theme(cx);

    // Out-of-bounds tint: a wash over the plot area when a visible trace has an
    // active alarm. Painted before the gridlines so they stay legible on top.
    if let Some(tint) = tint {
        window.paint_quad(gpui::fill(pb, tint));
    }
    let anchor = x_tick_anchor(fmt, data_start);
    // Horizontal gridlines and the zero line track the primary axis only;
    // secondary axes would draw conflicting lines across the same plot.
    let primary = view.axis_bounds(0);

    for tick in value_ticks(primary.min_y, primary.max_y, 5) {
        let y = primary.to_screen(pb, primary.min_x, tick).y;
        if y < pb.origin.y || y > pb.origin.y + pb.size.height {
            continue;
        }
        let mut grid = PathBuilder::stroke(px(0.5));
        grid.move_to(point(pb.origin.x, y));
        grid.line_to(point(pb.origin.x + pb.size.width, y));
        if let Ok(path) = grid.build() {
            window.paint_path(path, theme.grid_color);
        }
    }
    for tick in x_ticks(&primary, 5, anchor) {
        let x = primary.to_screen(pb, tick as f64, primary.min_y).x;
        if x < pb.origin.x || x > pb.origin.x + pb.size.width {
            continue;
        }
        let mut grid = PathBuilder::stroke(px(0.5));
        grid.move_to(point(x, pb.origin.y));
        grid.line_to(point(x, pb.origin.y + pb.size.height));
        if let Ok(path) = grid.build() {
            window.paint_path(path, theme.grid_color);
        }
    }

    if primary.min_y < 0.0 && primary.max_y > 0.0 {
        let zero_y = primary.to_screen(pb, primary.min_x, 0.0).y;
        let mut zp = PathBuilder::stroke(px(1.0));
        zp.move_to(point(pb.origin.x, zero_y));
        zp.line_to(point(pb.origin.x + pb.size.width, zero_y));
        if let Ok(path) = zp.build() {
            window.paint_path(path, theme.zero_line_color);
        }
    }
}

/// Paint axis chrome on top of the GPU-rendered line plot.
///
/// Renders the semi-transparent axis fills (which mask sub-pixel GPU-frame
/// bleeds into the chrome), tick labels, and the L-shaped axes.
#[allow(clippy::too_many_arguments)]
fn paint_overlay(
    outer_bounds: Bounds<Pixels>,
    view: &PlotView,
    axis_colors: &[Option<Hsla>],
    axis_markers: &[(usize, f64, Hsla)],
    limit_lines: &[(usize, f64, Hsla, SharedString)],
    data_start: f64,
    fmt: TimeFormat,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let axis_count = view.axis_count();
    let pb = plot_area(outer_bounds, axis_count);
    let theme = crate::theme::theme(cx);

    let axis_bg = theme.plot_chrome_bg();
    let y_axis_bg = Bounds {
        origin: outer_bounds.origin,
        size: gpui::Size {
            width: pb.origin.x - outer_bounds.origin.x,
            height: outer_bounds.size.height,
        },
    };
    window.paint_quad(gpui::fill(y_axis_bg, axis_bg));

    let x_axis_bg = Bounds {
        origin: point(pb.origin.x, pb.origin.y + pb.size.height),
        size: gpui::Size {
            width: pb.size.width,
            height: outer_bounds.origin.y + outer_bounds.size.height - pb.origin.y - pb.size.height,
        },
    };
    window.paint_quad(gpui::fill(x_axis_bg, axis_bg));

    // Axes are neutral unless given an explicit color.
    let label_color = |i: usize| -> Hsla {
        axis_colors
            .get(i)
            .copied()
            .flatten()
            .unwrap_or(theme.text_secondary)
    };
    let rule_color = |i: usize| -> Hsla {
        axis_colors
            .get(i)
            .copied()
            .flatten()
            .unwrap_or(theme.axis_color)
    };

    // Per-axis Y tick labels, right-aligned within each stacked column.
    for i in 0..axis_count {
        let ab = view.axis_bounds(i);
        let color = label_color(i);
        // Right edge of column `i` (column 0 abuts the plot, higher indices
        // step left by Y_LABEL_WIDTH each).
        let col_right = pb.origin.x - px(i as f32 * Y_LABEL_WIDTH);
        for tick in value_ticks(ab.min_y, ab.max_y, 5) {
            let y = ab.to_screen(pb, ab.min_x, tick).y;
            if y < pb.origin.y || y > pb.origin.y + pb.size.height {
                continue;
            }
            // Right-aligned 4px clear of the axis rule; a label wider than
            // its column pins to the pane edge instead of painting over the
            // tile border (this canvas draws above the tile chrome).
            crate::views::plot_common::paint_value_label(
                tick,
                color,
                |width, font_size| {
                    let x = (col_right - width - px(4.0)).max(outer_bounds.origin.x + px(2.0));
                    point(x, y - font_size / 2.0)
                },
                window,
                cx,
            );
        }
    }

    // X tick labels track the primary axis's X range.
    let primary = view.axis_bounds(0);
    let t_min_i = data_start as i64;
    let anchor = x_tick_anchor(fmt, data_start);
    let span_us = primary.width();
    for tick in x_ticks(&primary, 5, anchor) {
        let x = primary.to_screen(pb, tick as f64, primary.min_y).x;
        if x < pb.origin.x || x > pb.origin.x + pb.size.width {
            continue;
        }
        let text = format_time_label(tick, t_min_i, fmt, span_us);
        // Centered on the tick, but a label near an edge tick is pulled
        // inside the pane so it can't run over the tile border — the right
        // chrome is only PADDING wide, far narrower than a time label.
        crate::views::plot_common::paint_text_label(
            text,
            theme.text_secondary,
            |width, _| {
                let label_x = (x - width / 2.0).clamp(
                    outer_bounds.origin.x + px(2.0),
                    outer_bounds.origin.x + outer_bounds.size.width - width - px(2.0),
                );
                point(label_x, pb.origin.y + pb.size.height + px(4.0))
            },
            window,
            cx,
        );
    }

    // One vertical rule per axis at its column's right edge, colored to
    // match; plus the shared horizontal X rule along the bottom.
    for i in 0..axis_count {
        let col_right = pb.origin.x - px(i as f32 * Y_LABEL_WIDTH);
        let mut rule = PathBuilder::stroke(px(1.0));
        rule.move_to(point(col_right, pb.origin.y));
        rule.line_to(point(col_right, pb.origin.y + pb.size.height));
        if let Ok(path) = rule.build() {
            window.paint_path(path, rule_color(i));
        }
    }

    // In multi-axis mode, mark each trace's axis membership with a small
    // triangle pointing into the plot at the line's left-edge value, drawn
    // in that axis's coordinate frame so it sits where the line begins.
    if axis_count > 1 {
        let tip_w = px(6.0);
        let half_h = px(4.0);
        for &(axis_i, value, color) in axis_markers {
            let ab = view.axis_bounds(axis_i);
            let y = ab.to_screen(pb, ab.min_x, value).y;
            if y < pb.origin.y || y > pb.origin.y + pb.size.height {
                continue;
            }
            let tip_x = pb.origin.x - px(axis_i as f32 * Y_LABEL_WIDTH);
            let base_x = tip_x - tip_w;
            let mut tri = PathBuilder::fill();
            tri.move_to(point(tip_x, y));
            tri.line_to(point(base_x, y - half_h));
            tri.line_to(point(base_x, y + half_h));
            tri.line_to(point(tip_x, y));
            if let Ok(path) = tri.build() {
                window.paint_path(path, color);
            }
        }
    }
    // Alarm limit lines: horizontal rules at each declared threshold, drawn in the
    // owning axis's coordinate frame, with the threshold label at the right edge.
    for (axis_i, value, color, label) in limit_lines {
        let ab = view.axis_bounds(*axis_i);
        let y = ab.to_screen(pb, ab.min_x, *value).y;
        if y < pb.origin.y || y > pb.origin.y + pb.size.height {
            continue;
        }
        let mut line = PathBuilder::stroke(px(1.0));
        line.move_to(point(pb.origin.x, y));
        line.line_to(point(pb.origin.x + pb.size.width, y));
        if let Ok(path) = line.build() {
            window.paint_path(path, *color);
        }
        if !label.is_empty() {
            crate::views::plot_common::paint_text_label(
                label.clone(),
                *color,
                |width, font_size| {
                    point(
                        pb.origin.x + pb.size.width - width - px(4.0),
                        y - font_size - px(2.0),
                    )
                },
                window,
                cx,
            );
        }
    }

    let mut x_rule = PathBuilder::stroke(px(1.0));
    x_rule.move_to(point(pb.origin.x, pb.origin.y + pb.size.height));
    x_rule.line_to(point(
        pb.origin.x + pb.size.width,
        pb.origin.y + pb.size.height,
    ));
    if let Ok(path) = x_rule.build() {
        window.paint_path(path, theme.axis_color);
    }
}

/// Snapshot of one cursor's visible state, captured during canvas prepare.
///
/// Keeps the paint closure free of any `App` access so the gpui callback can
/// stay `'static + Fn` without re-entering the entity tree on every frame.
struct CursorPaint {
    t_start: Timestamp,
    t_end: Timestamp,
    /// `true` for the cursor currently being dragged out. Drives the bright
    /// selection-color stroke; persistent cursors render muted.
    active: bool,
    /// `(norm_start, norm_end, color)` per visible trace: the trace's value
    /// at `t_start`/`t_end` already normalized to `[0,1]` against its axis,
    /// so the overlay drops dot markers using a fixed `0..1` Y mapping.
    trace_markers: Vec<(f64, f64, Hsla)>,
}

/// Build a marker rect whose edges land on physical pixels. Plot projection
/// commonly produces fractional logical coordinates; feeding those directly
/// to a rounded quad can make opposite corners cover different pixel samples.
fn marker_bounds(center: Point<Pixels>, scale_factor: f32) -> Bounds<Pixels> {
    let snap = |value: Pixels| px((f32::from(value) * scale_factor).round() / scale_factor);
    let left = snap(center.x - px(3.0));
    let top = snap(center.y - px(3.0));
    let right = snap(center.x + px(3.0));
    let bottom = snap(center.y + px(3.0));
    Bounds::new(point(left, top), gpui::size(right - left, bottom - top))
}

fn paint_cursors(
    outer_bounds: Bounds<Pixels>,
    view: &PlotView,
    cursors: &[CursorPaint],
    window: &mut Window,
    cx: &mut gpui::App,
) {
    if cursors.is_empty() {
        return;
    }
    let pb = plot_area(outer_bounds, view.axis_count());
    // Lines use X only; markers carry normalized [0,1] Y, so a 0..1 Y view
    // maps them straight to screen.
    let view = view.x_bounds();
    let theme = crate::theme::theme(cx);
    let scale_factor = window.scale_factor();

    for cursor in cursors {
        let (a, b) = if cursor.t_start.0 <= cursor.t_end.0 {
            (cursor.t_start, cursor.t_end)
        } else {
            (cursor.t_end, cursor.t_start)
        };

        let primary = if cursor.active {
            theme.selection_bg
        } else {
            theme.text_secondary
        };
        // Brighten the selection-derived stroke so it reads as a line, not a fill.
        let stroke_color = Hsla { a: 1.0, ..primary };

        for ts in [a, b] {
            let x = view.to_screen(pb, ts.0 as f64, view.min_y).x;
            if x < pb.origin.x - px(1.0) || x > pb.origin.x + pb.size.width + px(1.0) {
                continue;
            }
            let mut line = PathBuilder::stroke(px(1.0));
            line.move_to(point(x, pb.origin.y));
            line.line_to(point(x, pb.origin.y + pb.size.height));
            if let Ok(path) = line.build() {
                window.paint_path(path, stroke_color);
            }
        }

        // Shaded band between the two endpoints; subtle so the underlying
        // traces stay legible.
        let xa = view.to_screen(pb, a.0 as f64, view.min_y).x;
        let xb = view.to_screen(pb, b.0 as f64, view.min_y).x;
        let band_left = xa.max(pb.origin.x);
        let band_right = xb.min(pb.origin.x + pb.size.width);
        if band_right > band_left {
            let band = Bounds {
                origin: point(band_left, pb.origin.y),
                size: gpui::Size {
                    width: band_right - band_left,
                    height: pb.size.height,
                },
            };
            let band_fill = Hsla {
                a: 0.08,
                ..stroke_color
            };
            window.paint_quad(gpui::fill(band, band_fill));
        }

        // Dot marker per visible trace at each endpoint.
        for (start_value, end_value, color) in &cursor.trace_markers {
            for (ts, value) in [(a, *start_value), (b, *end_value)] {
                let screen = view.to_screen(pb, ts.0 as f64, value);
                if screen.x < pb.origin.x
                    || screen.x > pb.origin.x + pb.size.width
                    || screen.y < pb.origin.y
                    || screen.y > pb.origin.y + pb.size.height
                {
                    continue;
                }
                let dot = marker_bounds(screen, scale_factor);
                let mut dot_quad = gpui::fill(dot, *color);
                dot_quad.corner_radii = gpui::Corners::all(px(1.5));
                window.paint_quad(dot_quad);
            }
        }
    }
}

/// Snapshot of the hover overlay's visible state, captured during canvas
/// prepare (mirrors [`CursorPaint`]) so the paint closure stays free of
/// entity access.
struct HoverPaint {
    pointer_x: Pixels,
    /// `(ts_us, norm_y, color)` per visible trace: the nearest sample's
    /// timestamp with its value pre-normalized to `[0,1]` against the
    /// trace's axis, so paint maps it with a fixed `0..1` Y view.
    markers: Vec<(f64, f64, Hsla)>,
}

/// Paint the plain-hover crosshair: one thin vertical rule at the pointer's
/// X, clipped to the plot area.
///
/// Deliberately thinner and more muted than the measurement cursors — it is
/// transient chrome, never a persisted selection line.
fn paint_hover_crosshair(
    outer_bounds: Bounds<Pixels>,
    view: &PlotView,
    pointer_x: Pixels,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let pb = plot_area(outer_bounds, view.axis_count());
    if pointer_x < pb.origin.x || pointer_x > pb.origin.x + pb.size.width {
        return;
    }
    let color = Hsla {
        a: 0.6,
        ..crate::theme::theme(cx).text_secondary
    };
    let mut line = PathBuilder::stroke(px(0.5));
    line.move_to(point(pointer_x, pb.origin.y));
    line.line_to(point(pointer_x, pb.origin.y + pb.size.height));
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
}

/// Paint one dot per visible trace at the sample the hover readout reports,
/// pinning the readout's values to the data points they describe. Same dot
/// idiom as the measurement cursors' endpoint markers in [`paint_cursors`].
fn paint_hover_markers(
    outer_bounds: Bounds<Pixels>,
    view: &PlotView,
    markers: &[(f64, f64, Hsla)],
    window: &mut Window,
) {
    if markers.is_empty() {
        return;
    }
    let pb = plot_area(outer_bounds, view.axis_count());
    // Markers carry normalized [0,1] Y, so a 0..1 Y view maps them straight
    // to screen.
    let view = view.x_bounds();
    let scale_factor = window.scale_factor();
    for &(ts, norm_y, color) in markers {
        let screen = view.to_screen(pb, ts, norm_y);
        if screen.x < pb.origin.x
            || screen.x > pb.origin.x + pb.size.width
            || screen.y < pb.origin.y
            || screen.y > pb.origin.y + pb.size.height
        {
            continue;
        }
        let dot = marker_bounds(screen, scale_factor);
        let mut dot_quad = gpui::fill(dot, color);
        dot_quad.corner_radii = gpui::Corners::all(px(1.5));
        window.paint_quad(dot_quad);
    }
}

/// Rendering mode for a single [`Trace`].
#[derive(Clone, Copy, Default, PartialEq, facet::Facet, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum PlotStyle {
    #[default]
    Line,
    Scatter,
    Bar,
}

impl PlotStyle {
    pub const ALL: [PlotStyle; 3] = [PlotStyle::Line, PlotStyle::Scatter, PlotStyle::Bar];

    pub fn label(self) -> &'static str {
        match self {
            PlotStyle::Line => "Line",
            PlotStyle::Scatter => "Scatter",
            PlotStyle::Bar => "Bar",
        }
    }

    pub fn parse(s: &str) -> Option<PlotStyle> {
        match s.to_lowercase().as_str() {
            "line" => Some(PlotStyle::Line),
            "scatter" => Some(PlotStyle::Scatter),
            "bar" => Some(PlotStyle::Bar),
            _ => None,
        }
    }
}

/// How the X axis renders timestamps.
///
/// `Relative` (the default) labels ticks as offsets from the earliest
/// sample (`"0"`, `"1 min 30 s"`). The absolute variants render wall-clock
/// time so events can be read against real-world clocks; `Utc` uses UTC and
/// `Local` the machine's timezone. Data positions are unchanged — only the
/// tick labels and their anchoring differ.
#[derive(
    Clone, Copy, Default, PartialEq, Eq, Debug, facet::Facet, serde::Serialize, serde::Deserialize,
)]
#[repr(u8)]
pub enum TimeFormat {
    #[default]
    Relative,
    Utc,
    Local,
}

/// One series on a plot: a single element of a component with its own
/// color, style, and label.
#[derive(Clone, facet::Facet)]
#[facet(pod)]
pub struct Trace {
    #[facet(skip)]
    pub component_id: ComponentId,
    #[facet(skip)]
    pub element_index: usize,
    pub color: Hsla,
    pub style: PlotStyle,
    pub visible: bool,
    pub label: SharedString,
    #[facet(inspect::range(min = "0.5", max = "10.0"))]
    pub stroke_width: f32,
    /// Index into the owning plot's `axes`. Edited via the axis picker, not
    /// reflected directly. Clamped to a valid axis by the plot's reconcile.
    #[facet(skip)]
    pub axis_index: usize,
    /// Back-reference to the owning plot, set by [`LinePlot::reconcile`].
    /// Lets the per-trace axis picker enumerate sibling axes (mirrors
    /// `MeasurementCursor`'s plot handle). `None` until reconcile runs.
    #[facet(opaque)]
    pub line_plot: Option<gpui::WeakEntity<LinePlot>>,
}

impl Trace {
    pub fn new(component_id: impl Into<ComponentId>, element_index: usize, color: Hsla) -> Self {
        Self {
            component_id: component_id.into(),
            element_index,
            color,
            style: PlotStyle::default(),
            visible: true,
            label: SharedString::new_static(""),
            stroke_width: 1.5,
            axis_index: 0,
            line_plot: None,
        }
    }
}

/// State captured at the moment a right-click measurement drag begins.
///
/// `start_view` is snapshotted so the visible bounds stay frozen across the
/// drag — the same pattern the left-click pan uses to keep coordinates
/// stable while the user is reaching for an endpoint.
struct CursorDrag {
    cursor: Entity<MeasurementCursor>,
    start_view: PlotView,
}

/// Interactive wrapper around a [`LinePlot`] that adds axes, legend, and
/// pan/zoom input.
///
/// All plot state — traces, bounds tracking, view overrides, GPU resources
/// — lives in the inner [`LinePlot`] (also the Facet inspection target).
/// `TimeSeriesPlot` only owns drag state and chrome.
pub struct TimeSeriesPlot {
    line_plot: Entity<LinePlot>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotView>,
    drag_zone: AxisZone,
    last_plot_area: Option<Bounds<Pixels>>,
    cursors: smallvec::SmallVec<[Entity<MeasurementCursor>; 2]>,
    cursor_drag: Option<CursorDrag>,
    panel_position: PanelPosition,
    /// Pointer-to-panel-origin offset captured at drag start, in window
    /// coordinates. `Some` while a panel drag is in flight.
    panel_drag: Option<Point<Pixels>>,
    /// Window-local pointer position while plain-hovering over the plot,
    /// driving the crosshair and value readout. `None` unless a bare hover
    /// (no modifier, no drag) is over the plot area.
    hover: Option<Point<Pixels>>,
    /// Enabled event sources resolved once per frame in `render`, so the flag
    /// canvas can query them without touching `&mut App` during paint.
    frame_sources: Vec<Rc<dyn EventSource>>,
    /// Repaint subscriptions on the enabled sources, refreshed only when the
    /// enabled key set (`event_sub_keys`) changes.
    event_subs: Vec<Subscription>,
    event_sub_keys: Vec<EventKindKey>,
    /// This frame's flag clusters, cached by the flag canvas for gutter
    /// hit-testing and the popovers.
    event_clusters: Vec<EventCluster>,
    /// Index into `event_clusters` of the flag under the pointer, or `None`.
    /// Transient — recomputed on every gutter hover.
    hovered_event_cluster: Option<usize>,
    /// The pinned flag popover, keyed by timestamp (clusters rebuild per frame).
    pinned_event: Option<PinnedEvent>,
    /// Lazily-built tree for a pinned event whose detail is decoded JSON.
    event_json_tree: Option<Entity<JsonTree>>,
}

/// A pinned event popover: the flag it opened on (by timestamp, since the
/// cluster list rebuilds each frame) and which of its events is selected.
struct PinnedEvent {
    x_ts: i64,
    selected: usize,
}

/// Build traces for `component_id` × `indices`, cycling theme colors and
/// labelling each trace `component.element`. Shared by tile and inspector
/// preview construction so both paths look identical.
pub fn traces_for_component(
    db: &DB,
    component_id: ComponentId,
    indices: &[usize],
    cx: &gpui::App,
) -> Vec<Trace> {
    let theme = crate::theme::theme(cx);
    let elem_names = crate::inspector::trace_picker::element_names_for_component(db, component_id);
    let comp_name = db
        .with_state(|s| {
            s.get_component_metadata(component_id)
                .map(|m| m.name.clone())
        })
        .unwrap_or_default();
    indices
        .iter()
        .enumerate()
        .map(|(i, &idx)| {
            let label = elem_names
                .get(idx)
                .map(|n| format!("{}.{}", comp_name, n))
                .unwrap_or_else(|| format!("{}[{}]", comp_name, idx));
            Trace {
                component_id,
                element_index: idx,
                color: theme.line_colors[i % theme.line_colors.len()],
                style: PlotStyle::default(),
                visible: true,
                label: SharedString::from(label),
                stroke_width: 1.5,
                axis_index: 0,
                line_plot: None,
            }
        })
        .collect()
}

impl TimeSeriesPlot {
    pub fn new(db: Arc<DB>, traces: Vec<Trace>, cx: &mut Context<Self>) -> Self {
        let line_plot = cx.new(|cx| {
            let mut lp = LinePlot::new(db, cx);
            lp.bind_traces(traces, cx);
            lp
        });
        // Legend and chrome live on this entity; listen to the inner
        // LinePlot so trace toggles and inspector edits trigger a repaint.
        cx.observe(&line_plot, |_, _, cx| cx.notify()).detach();
        // Repaint when alarms change so limit lines and out-of-bounds tinting
        // track the control system's broadcast.
        if let Some(store) = crate::alarms::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            line_plot,
            drag_start: None,
            drag_start_view: None,
            drag_zone: AxisZone::Plot,
            last_plot_area: None,
            cursors: smallvec::SmallVec::new(),
            cursor_drag: None,
            panel_position: PanelPosition::default(),
            panel_drag: None,
            hover: None,
            frame_sources: Vec::new(),
            event_subs: Vec::new(),
            event_sub_keys: Vec::new(),
            event_clusters: Vec::new(),
            hovered_event_cluster: None,
            pinned_event: None,
            event_json_tree: None,
        }
    }

    pub fn panel_position(&self) -> PanelPosition {
        self.panel_position
    }

    pub fn last_plot_area(&self) -> Option<Bounds<Pixels>> {
        self.last_plot_area
    }

    pub fn set_panel_position(&mut self, position: PanelPosition, cx: &mut Context<Self>) {
        if self.panel_position != position {
            self.panel_position = position;
            cx.notify();
        }
    }

    /// Start dragging a panel from its current screen origin.
    ///
    /// Called by both per-cursor mini panels (which pass the cursor's
    /// band-anchored origin) and the consolidated panel (which passes its
    /// current pinned origin). Transitions to `Pinned` at that origin so
    /// the drag has a stable starting point regardless of which mode the
    /// panel started in.
    ///
    /// `mouse` is window-local; `panel_pa_local` is plot-area-local. Both
    /// are rounded to integer pixels so the f32 storage stays aligned with
    /// taffy's rounded layout output and the panel doesn't drift sub-pixel
    /// during the drag.
    pub fn start_panel_drag(
        &mut self,
        mouse: Point<Pixels>,
        panel_pa_local: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(plot_area) = self.last_plot_area else {
            return;
        };
        let pa_local = Point {
            x: panel_pa_local.x.round(),
            y: panel_pa_local.y.round(),
        };
        let panel_window = plot_area.origin + pa_local;
        self.panel_position = PanelPosition::Pinned {
            x: pa_local.x.into(),
            y: pa_local.y.into(),
        };
        self.panel_drag = Some(mouse - panel_window);
        cx.notify();
    }

    /// Click-without-drag handler for a per-cursor mini-panel's lock icon:
    /// pin the consolidated panel at this cursor's current band position.
    pub fn pin_panel_at(&mut self, panel_pa_local: Point<Pixels>, cx: &mut Context<Self>) {
        self.set_panel_position(
            PanelPosition::Pinned {
                x: panel_pa_local.x.round().into(),
                y: panel_pa_local.y.round().into(),
            },
            cx,
        );
    }

    /// Click handler for the consolidated panel's track icon: return to
    /// per-cursor track mode.
    pub fn unpin_panel(&mut self, cx: &mut Context<Self>) {
        self.set_panel_position(PanelPosition::Track, cx);
    }

    /// Advance the panel drag to a new mouse position. Called from both the
    /// plot wrapper's `on_mouse_move` (fires when the cursor is over bare
    /// plot area) and each panel's own `on_mouse_move` (fires when the
    /// cursor is over the panel — the panel `.occlude()`s the wrapper so
    /// the wrapper's listener doesn't fire while the cursor is on the
    /// panel). Without both, the panel only moves once the cursor leaves
    /// it onto plot area.
    ///
    /// Returns `true` when a drag was in flight; the caller uses that to
    /// short-circuit other drag branches.
    pub fn advance_panel_drag(&mut self, mouse: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(offset) = self.panel_drag else {
            return false;
        };
        let Some(plot_area) = self.last_plot_area else {
            return true;
        };
        // gpui's taffy layout engine rounds positions to integer pixels
        // (taffy.rs:42 enable_rounding). Round here so each mouse-pixel
        // produces exactly one panel-pixel step rather than drifting in
        // and out of the rounding grid.
        let new_pa = (mouse - offset - plot_area.origin).map(|p| p.round());
        self.set_panel_position(
            PanelPosition::Pinned {
                x: new_pa.x.into(),
                y: new_pa.y.into(),
            },
            cx,
        );
        true
    }

    /// Clear an in-flight panel drag. Returns `true` when a drag was
    /// active so the caller can short-circuit fall-through handlers.
    pub fn end_panel_drag(&mut self) -> bool {
        self.panel_drag.take().is_some()
    }

    /// Currently-rendered measurement cursors, including the in-progress
    /// drag cursor (the last entry while `cursor_drag` is `Some`).
    pub fn cursors(&self) -> &[Entity<MeasurementCursor>] {
        &self.cursors
    }

    /// Replace the cursor list (used by [`PlotPanel::from_config`] to
    /// rehydrate locked cursors after layout deserialization). Each cursor
    /// is observed so the plot repaints when its endpoints or enabled
    /// measurements change.
    pub fn set_cursors(
        &mut self,
        cursors: smallvec::SmallVec<[Entity<MeasurementCursor>; 2]>,
        cx: &mut Context<Self>,
    ) {
        self.cursors = cursors;
        for cursor in self.cursors.clone() {
            cx.observe(&cursor, |_, _, cx| cx.notify()).detach();
        }
        cx.notify();
    }

    /// Rehydrate persisted cursors from saved config. Trace-index references
    /// are clamped to the current trace list, and out-of-range indices fall
    /// back to "no focused trace" so the cursor still renders.
    pub fn restore_cursors(&mut self, cfg: &[MeasurementCursorConfig], cx: &mut Context<Self>) {
        let lp_weak = self.line_plot.downgrade();
        let host_weak = cx.entity().downgrade();
        let traces = self.line_plot.read(cx).traces().to_vec();
        let mut restored: smallvec::SmallVec<[Entity<MeasurementCursor>; 2]> =
            smallvec::SmallVec::new();
        for entry in cfg {
            let focused_trace = if entry.focused_trace_index >= 0 {
                traces
                    .get(entry.focused_trace_index as usize)
                    .map(|t| t.entity_id())
            } else {
                None
            };
            let enabled: MeasurementKindList = entry.enabled.iter().copied().collect();
            let cursor = cx.new(|_| {
                MeasurementCursor::new(
                    Timestamp(entry.t_start_us),
                    Timestamp(entry.t_end_us),
                    focused_trace,
                    enabled,
                    lp_weak.clone(),
                    host_weak.clone(),
                )
            });
            restored.push(cursor);
        }
        self.set_cursors(restored, cx);
    }

    /// Drop the cursor matching `id`. No-op when the id has already been
    /// removed (the inspector close path races with explicit "Delete").
    pub fn remove_cursor(&mut self, id: CursorId, cx: &mut Context<Self>) {
        let len_before = self.cursors.len();
        self.cursors.retain(|c| c.read(cx).id != id);
        if self.cursors.len() != len_before {
            cx.notify();
        }
    }

    /// Convenience constructor: one trace per element, colors cycled from
    /// the theme's categorical palette, labels derived from element names.
    /// Empty `indexes` defaults to `[0]` to preserve historic behaviour.
    pub fn from_component(
        db: Arc<DB>,
        component_id: impl Into<ComponentId>,
        indexes: &[usize],
        cx: &mut Context<Self>,
    ) -> Self {
        let component_id = component_id.into();
        let indexes = if indexes.is_empty() {
            &[0usize] as &[usize]
        } else {
            indexes
        };
        let traces = traces_for_component(&db, component_id, indexes, cx);
        Self::new(db, traces, cx)
    }

    pub fn line_plot(&self) -> &Entity<LinePlot> {
        &self.line_plot
    }

    pub fn title(&self, cx: &gpui::App) -> SharedString {
        self.line_plot.read(cx).title()
    }

    /// Current effective view (auto-fit unless the user has overridden it).
    pub fn view(&self, cx: &gpui::App) -> Option<PlotView> {
        self.line_plot.read(cx).effective_view(cx)
    }

    /// Right-click hit-tests existing cursors; landing within a few pixels
    /// of a cursor line re-opens its inspector. Used by both the new-cursor
    /// path (after release) and the bare right-click on a locked cursor.
    fn open_cursor_inspector_at(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pa) = self.last_plot_area else {
            return;
        };
        let axis_count = self.line_plot.read(cx).axes.len();
        if axis_zone(position, pa, axis_count) != AxisZone::Plot {
            return;
        }
        let Some(view) = self.line_plot.read(cx).effective_view(cx) else {
            return;
        };
        let Some(hit) = cursor::cursor_at(&self.cursors, position.x, pa, view.x_bounds(), cx)
        else {
            return;
        };
        let entity = hit.into_any();
        window.defer(cx, move |window, cx| {
            window.dispatch_action(
                Box::new(crate::inspector::InspectEntity { entity, position }),
                cx,
            );
        });
    }

    fn start_cursor_drag(
        &mut self,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pa) = self.last_plot_area else {
            return;
        };
        let axis_count = self.line_plot.read(cx).axes.len();
        if axis_zone(event.position, pa, axis_count) != AxisZone::Plot {
            return;
        }
        let Some(view) = self.line_plot.read(cx).effective_view(cx) else {
            return;
        };

        // Pin bounds for the duration of the drag (mirrors left-drag pan).
        self.line_plot
            .update(cx, |lp, cx| lp.set_view_override(Some(view.clone()), cx));

        let x_data = cursor::pixel_to_data_x(event.position.x, pa, view.x_bounds());
        let focused = cursor::focused_trace_at(
            self.line_plot.read(cx),
            Timestamp(x_data as i64),
            event.position.y,
            pa,
            &view,
            cx,
        );
        let snapped = cursor::snap_x_to_sample(self.line_plot.read(cx), focused, x_data, cx);
        let enabled = MeasurementKindList::from_slice(&[
            MeasurementKind::DeltaX,
            MeasurementKind::DeltaY,
            MeasurementKind::Mean,
        ]);
        let lp_weak = self.line_plot.downgrade();
        let host_weak = cx.entity().downgrade();
        let cursor_entity = cx.new(|_| {
            MeasurementCursor::new(snapped, snapped, focused, enabled, lp_weak, host_weak)
        });
        cx.observe(&cursor_entity, |_, _, cx| cx.notify()).detach();
        self.cursors.push(cursor_entity.clone());
        self.cursor_drag = Some(CursorDrag {
            cursor: cursor_entity,
            start_view: view,
        });
        cx.notify();
    }

    fn handle_cursor_drag_move(&mut self, event: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        let (Some(pa), Some(drag)) = (self.last_plot_area, self.cursor_drag.as_ref()) else {
            return;
        };
        let start_view = drag.start_view.clone();
        let cursor_entity = drag.cursor.clone();
        let x_data = cursor::pixel_to_data_x(event.position.x, pa, start_view.x_bounds());
        let focused = cursor_entity.read(cx).focused_trace;
        let snapped = cursor::snap_x_to_sample(self.line_plot.read(cx), focused, x_data, cx);
        cursor_entity.update(cx, |c, cx| {
            if c.t_end != snapped {
                c.t_end = snapped;
                cx.notify();
            }
        });
    }

    fn finish_cursor_drag(
        &mut self,
        event: &gpui::MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.cursor_drag.take() else {
            return;
        };
        let cursor_entity = drag.cursor;
        let (start, end) = {
            let c = cursor_entity.read(cx);
            (c.t_start, c.t_end)
        };
        // Zero-length drag — treat as a click, drop the placeholder cursor.
        if start == end {
            let id = cursor_entity.read(cx).id;
            self.remove_cursor(id, cx);
            return;
        }
        let entity = cursor_entity.into_any();
        let position = event.position;
        window.defer(cx, move |window, cx| {
            window.dispatch_action(
                Box::new(crate::inspector::InspectEntity { entity, position }),
                cx,
            );
        });
    }

    /// Clear every user-imposed bound so auto-fit resumes on the next frame.
    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.line_plot.update(cx, |lp, cx| {
            lp.x_range = Override::Auto;
            for axis in lp.axes.clone() {
                axis.update(cx, |a, cx| {
                    a.y_min_override = Override::Auto;
                    a.y_max_override = Override::Auto;
                    cx.notify();
                });
            }
            lp.set_view_override(None, cx);
            cx.notify();
        });
    }

    /// Drop the hover crosshair/readout, repainting if it was showing.
    /// Called whenever a gesture that owns the pointer (pan, zoom, cursor
    /// drag, inspector) begins, and on mouse-leave.
    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        let had = self.hover.take().is_some() || self.hovered_event_cluster.take().is_some();
        if had {
            cx.notify();
        }
    }

    /// Track the bare-hover pointer for the crosshair and value readout.
    ///
    /// Mutually exclusive with every other pointer gesture: a held modifier
    /// (measurement), an in-flight drag, or a pointer over the axis chrome
    /// all suppress it, so hover never fights pan/zoom/measure.
    fn update_hover(&mut self, event: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        if event.modifiers.alt
            || self.cursor_drag.is_some()
            || self.drag_start.is_some()
            || self.panel_drag.is_some()
        {
            self.clear_hover(cx);
            return;
        }
        let Some(pa) = self.last_plot_area else {
            self.clear_hover(cx);
            return;
        };
        // The flag gutter takes precedence over trace hover, and suppresses the
        // normal value readout while the pointer is over a flag.
        if let Some(idx) = self.event_cluster_at(event.position, pa) {
            let changed = self.hovered_event_cluster != Some(idx) || self.hover.is_some();
            self.hovered_event_cluster = Some(idx);
            self.hover = None;
            if changed {
                cx.notify();
            }
            return;
        }
        if self.hovered_event_cluster.take().is_some() {
            cx.notify();
        }
        let axis_count = self.line_plot.read(cx).axes.len();
        if axis_zone(event.position, pa, axis_count) != AxisZone::Plot {
            self.clear_hover(cx);
            return;
        }
        self.hover = Some(event.position);
        cx.notify();
    }

    /// Resolve the hover crosshair against every visible trace: the pointer's
    /// data-space X plus each trace's nearest sample (via the shared
    /// [`LinePlot::trace_sample_at`]). The readout rows and the on-plot
    /// sample markers are both derived from this one lookup, so the
    /// highlighted point is always the sample the readout reports.
    fn hover_samples(&self, cx: &gpui::App) -> Option<(f64, Vec<HoverSample>)> {
        let pointer = self.hover?;
        let pa = self.last_plot_area?;
        let lp = self.line_plot.read(cx);
        let view = lp.effective_view(cx)?;
        let x_data = cursor::pixel_to_data_x(pointer.x, pa, view.x_bounds());
        let ts = Timestamp(x_data as i64);
        let mut samples = Vec::new();
        for trace in lp.traces() {
            let cfg = trace.read(cx);
            if !cfg.visible {
                continue;
            }
            let Some((sample_ts, value)) = lp.trace_sample_at(trace.entity_id(), ts, cx) else {
                continue;
            };
            samples.push(HoverSample {
                color: cfg.color,
                label: cfg.label.clone(),
                ts: sample_ts,
                value,
                axis_index: cfg.axis_index,
            });
        }
        Some((x_data, samples))
    }

    /// Build the hover readout: a native div listing each visible trace's
    /// value at the crosshair timestamp (via [`Self::hover_samples`]) under
    /// a formatted time header, offset and clamped clear of the pointer and
    /// pane edges.
    fn hover_readout(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let pointer = self.hover?;
        let pa = self.last_plot_area?;
        let lp = self.line_plot.read(cx);
        let view = lp.effective_view(cx)?;
        let theme = crate::theme::theme(cx);

        let (x_data, samples) = self.hover_samples(cx)?;
        let header = format_time_label(
            x_data as i64,
            lp.data_start().unwrap_or(0.0) as i64,
            lp.x_time_format,
            view.x_bounds().width(),
        );

        let rows: HoverRows = samples
            .into_iter()
            .map(|s| {
                (
                    s.color,
                    s.label,
                    SharedString::from(format_value_label(s.value)),
                )
            })
            .collect();

        let lens: smallvec::SmallVec<[(usize, usize); 4]> = rows
            .iter()
            .map(|(_, label, value)| (label.chars().count(), value.chars().count()))
            .collect();
        let size = cursor::estimate_readout_size(header.chars().count(), &lens);
        let origin = cursor::readout_origin(pointer, size, pa, px(HOVER_READOUT_OFFSET));
        // Position within the wrapper div, whose origin is inset from the
        // plot area by the left chrome and top padding.
        let inner_origin = point(
            pa.origin.x - px(left_margin(view.axis_count())),
            pa.origin.y - px(PADDING),
        );

        let mut boxed = div()
            .flex()
            .flex_col()
            .gap_y_0()
            .px(px(READOUT_PAD_X))
            .py(px(READOUT_PAD_Y))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(3.0))
            .child(
                div()
                    .text_size(px(LABEL_FONT_SIZE))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(header)),
            );
        for (color, label, value) in rows {
            boxed = boxed.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(div().w(px(8.0)).h(px(8.0)).rounded(px(2.0)).bg(color))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(LABEL_FONT_SIZE))
                            .text_color(theme.text_secondary)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(LABEL_FONT_SIZE))
                            .text_color(color)
                            .child(value),
                    ),
            );
        }

        Some(
            div()
                .absolute()
                .left(origin.x - inner_origin.x)
                .top(origin.y - inner_origin.y)
                .child(boxed)
                .into_any_element(),
        )
    }

    // -- Event flags ---------------------------------------------------------

    /// Resolve the enabled event sources for this frame and keep the repaint
    /// subscriptions in sync. Runs in `render`, not paint, because
    /// [`EventSourceRegistry::source_for`] needs `&mut App` to lazily spin up a
    /// message store and observing a source needs `&mut Context`.
    fn sync_event_sources(&mut self, cx: &mut Context<Self>) {
        let db = self.line_plot.read(cx).db().clone();
        let overlays = self.line_plot.read(cx).event_overlays.clone();
        let mut sources: Vec<Rc<dyn EventSource>> = Vec::new();
        let mut keys: Vec<EventKindKey> = Vec::new();
        for overlay in &overlays {
            let overlay = overlay.read(cx);
            if !overlay.visible {
                continue;
            }
            let key = overlay.key;
            if let Some(source) = EventSourceRegistry::source_for(key, &db, cx) {
                keys.push(key);
                sources.push(source);
            }
        }
        // Re-subscribe only when the enabled set changes; the handles
        // themselves are cheap and refreshed every frame regardless.
        if keys != self.event_sub_keys {
            self.event_subs.clear();
            for source in &sources {
                if let Some(target) = source.observe_target(cx)
                    && let Some(sub) = self.observe_event_target(target, cx)
                {
                    self.event_subs.push(sub);
                }
            }
            self.event_sub_keys = keys;
        }
        self.frame_sources = sources;
    }

    /// Observe an event source's backing store for repaints. `observe` needs a
    /// typed handle, so the erased target is matched against the known store
    /// types; an unrecognized store just goes unobserved.
    fn observe_event_target(
        &self,
        target: gpui::AnyEntity,
        cx: &mut Context<Self>,
    ) -> Option<Subscription> {
        if let Ok(store) = target.clone().downcast::<crate::logs::LogStore>() {
            return Some(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        if let Ok(store) = target.clone().downcast::<crate::alarms::AlarmStore>() {
            return Some(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        if let Ok(store) = target.clone().downcast::<crate::sequences::SequenceStore>() {
            return Some(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        if let Ok(store) = target.downcast::<crate::plot_events::MsgEventStore>() {
            return Some(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        None
    }

    /// Query this frame's sources over the visible window and cluster the
    /// results into flags, mapping each event's time to a screen x.
    fn build_event_clusters(
        &self,
        view: &PlotView,
        pa: Bounds<Pixels>,
        cx: &gpui::App,
    ) -> Vec<EventCluster> {
        if self.frame_sources.is_empty() {
            return Vec::new();
        }
        let range = Timestamp(view.x.0 as i64)..Timestamp(view.x.1 as i64);
        let mut events: Vec<PlotEvent> = Vec::new();
        for source in &self.frame_sources {
            events.extend(source.events_in(range.clone(), cx));
        }
        // Ascending by time so the screen x's are ascending (the transform is
        // monotonic), which is what the clustering walk assumes.
        events.sort_by_key(|e| e.ts.0);
        let x_bounds = view.x_bounds();
        let positioned: Vec<(Pixels, PlotEvent)> = events
            .into_iter()
            .map(|e| (x_bounds.to_screen(pa, e.ts.0 as f64, 0.0).x, e))
            .collect();
        cluster_events(positioned)
    }

    /// Index of the flag whose chip is under `pos`, when the pointer is
    /// inside the top gutter band: the estimated chip extent to the right of
    /// the rule hits, plus [`FLAG_HIT_PX`] of slack to its left. Confined to
    /// the gutter so trace hover/pan/zoom below is untouched.
    fn event_cluster_at(&self, pos: Point<Pixels>, pa: Bounds<Pixels>) -> Option<usize> {
        let top = pa.origin.y;
        if pos.y < top || pos.y > top + px(GUTTER_H) {
            return None;
        }
        if pos.x < pa.origin.x || pos.x > pa.origin.x + pa.size.width {
            return None;
        }
        let mut best: Option<(usize, f32)> = None;
        for (i, cluster) in self.event_clusters.iter().enumerate().rev() {
            let offset = f32::from(pos.x - cluster.x);
            let chip_w = f32::from(EventCluster::chip_width(
                cluster.chip_label().chars().count(),
            ));
            let hit = (-FLAG_HIT_PX..=chip_w).contains(&offset);
            // Prefer the flag whose rule is nearest, so a chip overdrawn by a
            // truncated neighbor still resolves to the closer rule.
            let dist = offset.abs();
            if hit && best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((i, dist));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Rebuild the JSON detail tree for the pinned selection when its detail
    /// decodes to JSON. Called on pin/selection change, not per frame, so the
    /// tree (and its notify) doesn't churn the window.
    fn sync_pinned_json_tree(&mut self, cx: &mut Context<Self>) {
        let value = self.pinned_event.as_ref().and_then(|pin| {
            let cluster = self
                .event_clusters
                .iter()
                .min_by_key(|c| (c.ts().0 - pin.x_ts).abs())?;
            match &cluster.events.get(pin.selected)?.detail {
                EventDetail::Json(value) => Some(value.clone()),
                _ => None,
            }
        });
        match value {
            Some(value) => match &self.event_json_tree {
                Some(tree) => tree.update(cx, |t, cx| t.set_value(value, cx)),
                None => self.event_json_tree = Some(cx.new(|cx| JsonTree::new(value, cx))),
            },
            None => self.event_json_tree = None,
        }
    }

    /// Hover popover for the flag under the pointer: header plus a summary row
    /// per event. Native div positioned below the gutter, clamped to the plot.
    fn event_hover_popover(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let idx = self.hovered_event_cluster?;
        let cluster = self.event_clusters.get(idx)?;
        let pa = self.last_plot_area?;
        let view = self.line_plot.read(cx).effective_view(cx)?;
        let theme = crate::theme::theme(cx);

        let count = cluster.events.len();
        let first = cluster.events.first()?;
        let header = if count > 1 {
            SharedString::from(format!("{} (and {} more)", first.label, count - 1))
        } else {
            first.label.clone()
        };

        let mut boxed = div()
            .flex()
            .flex_col()
            .gap_y_0()
            .px(px(READOUT_PAD_X))
            .py(px(READOUT_PAD_Y))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(3.0))
            .child(
                div()
                    .text_size(px(LABEL_FONT_SIZE))
                    .text_color(theme.text_secondary)
                    .child(header),
            );
        for ev in cluster.events.iter().take(EVENT_POPOVER_ROWS) {
            boxed = boxed.child(event_summary_row(ev, &theme));
        }

        let shown = count.min(EVENT_POPOVER_ROWS);
        let size = gpui::Size {
            width: px(240.0),
            height: px((1 + shown) as f32 * 16.0 + 8.0),
        };
        let anchor = point(cluster.x, pa.origin.y + px(GUTTER_H));
        let origin = cursor::readout_origin(anchor, size, pa, px(4.0));
        let inner_origin = point(
            pa.origin.x - px(left_margin(view.axis_count())),
            pa.origin.y - px(PADDING),
        );
        Some(
            div()
                .absolute()
                .left(origin.x - inner_origin.x)
                .top(origin.y - inner_origin.y)
                .child(boxed)
                .into_any_element(),
        )
    }

    /// Pinned popover: a taller, elevated panel with a close button, a
    /// scrollable event list whose rows select, and the selected event's typed
    /// detail below. Occludes the plot so clicks inside don't clear the pin.
    fn event_pinned_popover(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let pin = self.pinned_event.as_ref()?;
        let pa = self.last_plot_area?;
        let view = self.line_plot.read(cx).effective_view(cx)?;
        let theme = crate::theme::theme(cx);

        let cluster = self
            .event_clusters
            .iter()
            .min_by_key(|c| (c.ts().0 - pin.x_ts).abs())?;
        if cluster.events.is_empty() {
            return None;
        }
        let selected = pin.selected.min(cluster.events.len() - 1);

        // Header: title + close button.
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .text_size(px(LABEL_FONT_SIZE))
                    .text_color(theme.text_primary)
                    .child(SharedString::from(format!(
                        "{} events",
                        cluster.events.len()
                    ))),
            )
            .child(
                div()
                    .id("event-popover-close")
                    .cursor_pointer()
                    .text_size(px(LABEL_FONT_SIZE))
                    .text_color(theme.text_secondary)
                    .child(SharedString::new_static("✕"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event: &gpui::MouseDownEvent, _window, cx| {
                            this.pinned_event = None;
                            this.event_json_tree = None;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            );

        // Event list: one selectable row per event.
        let mut list = div()
            .id("event-popover-list")
            .flex()
            .flex_col()
            .max_h(px(320.0))
            .overflow_y_scroll();
        for (i, ev) in cluster.events.iter().enumerate() {
            let is_selected = i == selected;
            let row_bg = if is_selected {
                theme.selection_bg
            } else {
                gpui::transparent_black()
            };
            list = list.child(
                div()
                    .id(("event-popover-row", i))
                    .cursor_pointer()
                    .rounded(px(2.0))
                    .bg(row_bg)
                    .child(event_summary_row(ev, &theme))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event: &gpui::MouseDownEvent, _window, cx| {
                            if let Some(pin) = this.pinned_event.as_mut() {
                                pin.selected = i;
                            }
                            this.sync_pinned_json_tree(cx);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            );
        }

        let detail = self.event_detail_element(&cluster.events[selected].detail, &theme);

        let panel = div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(300.0))
            .px(px(READOUT_PAD_X))
            .py(px(READOUT_PAD_Y))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.selection_bg)
            .rounded(px(4.0))
            .occlude()
            .child(header)
            .child(list)
            .child(detail);

        let size = gpui::Size {
            width: px(300.0),
            height: px(360.0),
        };
        let anchor = point(cluster.x, pa.origin.y + px(GUTTER_H));
        let origin = cursor::readout_origin(anchor, size, pa, px(4.0));
        let inner_origin = point(
            pa.origin.x - px(left_margin(view.axis_count())),
            pa.origin.y - px(PADDING),
        );
        Some(
            div()
                .absolute()
                .left(origin.x - inner_origin.x)
                .top(origin.y - inner_origin.y)
                .child(panel)
                .into_any_element(),
        )
    }

    /// Typed detail block for the selected pinned event, reusing each store's
    /// own record fields (log-panel `key: value` styling).
    fn event_detail_element(
        &self,
        detail: &EventDetail,
        theme: &crate::theme::Theme,
    ) -> AnyElement {
        let mut body = div().flex().flex_col().gap_y_0().pt_1();
        match detail {
            EventDetail::Log(ev) => {
                body = body
                    .child(kv_row("level", format!("{:?}", ev.level), theme))
                    .child(kv_row("source", ev.source.clone(), theme))
                    .child(kv_row("message", ev.message.clone(), theme));
                for (k, v) in &ev.fields {
                    body = body.child(kv_row(k, v.clone(), theme));
                }
            }
            EventDetail::Alarm(ev) => {
                body = body
                    .child(kv_row("alarm", ev.def_id.clone(), theme))
                    .child(kv_row("severity", format!("{:?}", ev.severity), theme));
                if !ev.detail.is_empty() {
                    body = body.child(kv_row("detail", ev.detail.clone(), theme));
                }
            }
            EventDetail::Sequence(entry) => {
                body = body
                    .child(kv_row("channel", entry.channel_name.to_string(), theme))
                    .child(kv_row("event", entry.label.to_string(), theme));
            }
            EventDetail::Json(_) => {
                if let Some(tree) = &self.event_json_tree {
                    body = body.child(tree.clone());
                }
            }
            EventDetail::Raw(len) => {
                body = body.child(
                    div()
                        .text_size(px(LABEL_FONT_SIZE))
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(format!(
                            "{len} bytes (no schema announced)"
                        ))),
                );
            }
        }
        body.into_any_element()
    }
}

/// Rows shown in a flag popover before the list is elided.
const EVENT_POPOVER_ROWS: usize = 8;

/// One event summary line: time, a color dot, and the one-line label.
fn event_summary_row(ev: &PlotEvent, theme: &crate::theme::Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(
            div()
                .text_size(px(LABEL_FONT_SIZE))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(crate::views::log_panel::format_time(
                    ev.ts.0,
                ))),
        )
        .child(div().w(px(8.0)).h(px(8.0)).rounded(px(2.0)).bg(ev.color))
        .child(
            div()
                .flex_1()
                .text_size(px(LABEL_FONT_SIZE))
                .text_color(theme.text_secondary)
                .child(ev.label.clone()),
        )
}

/// One `key: value` detail row in the pinned popover.
fn kv_row(
    key: &str,
    value: impl Into<SharedString>,
    theme: &crate::theme::Theme,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .gap_1()
        .text_size(px(LABEL_FONT_SIZE))
        .child(
            div()
                .text_color(theme.text_tertiary)
                .child(SharedString::from(format!("{key}:"))),
        )
        .child(
            div()
                .flex_1()
                .text_color(theme.text_primary)
                .child(value.into()),
        )
}

impl Render for TimeSeriesPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        // Resolve this frame's event sources (and refresh their repaint subs)
        // before building canvases, so the flag canvas can query them in paint.
        self.sync_event_sources(cx);
        let trace_entities: Vec<Entity<Trace>> = self.line_plot.read(cx).traces().to_vec();
        let show_legend = trace_entities.len() >= 2;
        // Drives the left chrome width: one Y_LABEL_WIDTH column per axis.
        let axis_count = self.line_plot.read(cx).axes.len();
        let chrome_left = px(left_margin(axis_count));

        let underlay_lp = self.line_plot.clone();
        let overlay_lp = self.line_plot.clone();
        let cursors_lp = self.line_plot.clone();
        let cursors_weak = cx.entity().downgrade();
        let hover_weak = cx.entity().downgrade();
        let flags_weak = cx.entity().downgrade();

        let mut root = div().flex().flex_col().size_full().bg(theme.bg_secondary);

        let mut inner = div()
            .id((
                "time-series-plot",
                cx.entity().entity_id().as_u64() as usize,
            ))
            .flex_1()
            .min_h_0()
            .relative()
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if !*hovered {
                    this.clear_hover(cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    this.clear_hover(cx);
                    if event.click_count == 2 {
                        this.reset_view(cx);
                        return;
                    }
                    // Alt+left-drag opens a measurement cursor —
                    // gpui's mac trackpad backend delivers right-
                    // mouse-down but not the matching drag/up, so
                    // the gesture has to ride on the left button.
                    if event.modifiers.alt {
                        this.start_cursor_drag(event, window, cx);
                        return;
                    }
                    // A click on a flag pins its popover; any other plot click
                    // clears an open pin before falling through to pan setup.
                    if let Some(pa) = this.last_plot_area
                        && let Some(idx) = this.event_cluster_at(event.position, pa)
                    {
                        let x_ts = this.event_clusters[idx].ts().0;
                        this.pinned_event = Some(PinnedEvent { x_ts, selected: 0 });
                        this.sync_pinned_json_tree(cx);
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    if this.pinned_event.take().is_some() {
                        this.event_json_tree = None;
                        cx.notify();
                    }
                    let axis_count = this.line_plot.read(cx).axes.len();
                    let zone = this
                        .last_plot_area
                        .map(|pa| axis_zone(event.position, pa, axis_count))
                        .unwrap_or(AxisZone::Plot);
                    this.drag_start = Some(event.position);
                    this.drag_start_view = this.line_plot.read(cx).effective_view(cx);
                    this.drag_zone = zone;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, window, cx| {
                    if this.end_panel_drag() {
                        return;
                    }
                    if this.cursor_drag.is_some() {
                        this.finish_cursor_drag(event, window, cx);
                        return;
                    }
                    this.drag_start = None;
                    this.drag_start_view = None;
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    this.clear_hover(cx);
                    this.open_cursor_inspector_at(event.position, window, cx);
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                    if !event.dragging() {
                        this.update_hover(event, cx);
                        return;
                    }
                    if this.advance_panel_drag(event.position, cx) {
                        return;
                    }
                    if this.cursor_drag.is_some() {
                        this.handle_cursor_drag_move(event, cx);
                        return;
                    }
                    let (Some(start), Some(start_view), Some(pa)) = (
                        this.drag_start,
                        this.drag_start_view.clone(),
                        this.last_plot_area,
                    ) else {
                        return;
                    };

                    let dx = event.position.x - start.x;
                    let dy = event.position.y - start.y;
                    let (nx, ny) = start_view.x_bounds().screen_delta_to_norm(pa, dx, dy);
                    let new_view = match this.drag_zone {
                        AxisZone::Plot => start_view.offset_x(-nx).offset_y_all(ny),
                        AxisZone::XAxis => start_view.offset_x(-nx),
                        AxisZone::YAxis(i) => start_view.offset_axis_y(i, ny),
                    };
                    this.line_plot
                        .update(cx, |lp, cx| lp.set_view_override(Some(new_view), cx));
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    this.clear_hover(cx);
                    let Some(view) = this.line_plot.read(cx).effective_view(cx) else {
                        return;
                    };
                    let Some(pa) = this.last_plot_area else {
                        return;
                    };

                    let delta = event.delta.pixel_delta(px(20.0));
                    let zoom_amount = f32::from(-delta.y) as f64 / 200.0;
                    let factor = (1.0_f64 + zoom_amount).clamp(0.5, 2.0);

                    let zone = axis_zone(event.position, pa, view.axis_count());
                    let (ax, ay) = view.x_bounds().screen_anchor(pa, event.position);
                    let new_view = match zone {
                        AxisZone::Plot => view.zoom_x(factor, ax).zoom_y_all(factor, 1.0 - ay),
                        AxisZone::XAxis => view.zoom_x(factor, ax),
                        AxisZone::YAxis(i) => view.zoom_axis_y(i, factor, 1.0 - ay),
                    };
                    this.line_plot
                        .update(cx, |lp, cx| lp.set_view_override(Some(new_view), cx));
                    cx.stop_propagation();
                }),
            )
            .child(
                canvas(
                    {
                        let this = cx.entity().downgrade();
                        move |bounds, _window, cx| {
                            let lp = underlay_lp.read(cx);
                            let axis_count = lp.axes.len();
                            let view = lp.effective_view(cx);
                            let data_start = lp.data_start().unwrap_or(0.0);
                            let fmt = lp.x_time_format;
                            let tint = alarm_plot_tint(lp, cx);
                            let _ = this.update(cx, |this, _| {
                                this.last_plot_area = Some(plot_area(bounds, axis_count));
                            });
                            (bounds, view, data_start, fmt, tint)
                        }
                    },
                    move |_, (bounds, view, data_start, fmt, tint), window, cx| {
                        if let Some(view) = view {
                            paint_underlay(bounds, &view, data_start, fmt, tint, window, cx);
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                div()
                    .absolute()
                    .left(chrome_left)
                    .top(px(PADDING))
                    .right(px(PADDING))
                    .bottom(px(X_LABEL_HEIGHT + PADDING))
                    .child(self.line_plot.clone()),
            )
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        let lp = overlay_lp.read(cx);
                        let view = lp.effective_view(cx);
                        let colors = lp.axis_colors(cx);
                        // Per-trace axis markers: value at the visible left edge,
                        // only meaningful once there's more than one axis.
                        let mut markers: Vec<(usize, f64, Hsla)> = Vec::new();
                        if let Some(v) = &view
                            && v.axis_count() > 1
                        {
                            let left = Timestamp(v.x.0 as i64);
                            for trace in lp.traces() {
                                let cfg = trace.read(cx);
                                if !cfg.visible {
                                    continue;
                                }
                                if let Some(val) = lp.trace_value_at(trace.entity_id(), left, cx) {
                                    markers.push((cfg.axis_index, val, cfg.color));
                                }
                            }
                        }
                        let limit_lines = alarm_limit_lines(lp, cx);
                        (
                            bounds,
                            view,
                            colors,
                            markers,
                            limit_lines,
                            lp.data_start().unwrap_or(0.0),
                            lp.x_time_format,
                        )
                    },
                    move |_,
                          (bounds, view, colors, markers, limit_lines, data_start, fmt),
                          window,
                          cx| {
                        if let Some(view) = view {
                            paint_overlay(
                                bounds,
                                &view,
                                &colors,
                                &markers,
                                &limit_lines,
                                data_start,
                                fmt,
                                window,
                                cx,
                            );
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        let mut out = None;
                        let _ = flags_weak.update(cx, |this, cx| {
                            let Some(view) = this.line_plot.read(cx).effective_view(cx) else {
                                return;
                            };
                            let pa = plot_area(bounds, view.axis_count());
                            let clusters = this.build_event_clusters(&view, pa, cx);
                            let theme = crate::theme::theme(cx);
                            let paints: Vec<ClusterPaint> = clusters
                                .iter()
                                .map(|c| ClusterPaint {
                                    x: c.x,
                                    color: c.color(&theme),
                                    label: c.chip_label(),
                                })
                                .collect();
                            this.event_clusters = clusters;
                            out = Some((view, paints));
                        });
                        (bounds, out)
                    },
                    move |_, (bounds, data), window, cx| {
                        if let Some((view, paints)) = data {
                            paint_event_flags(bounds, &view, &paints, window, cx);
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        let view = cursors_lp.read(cx).effective_view(cx);
                        let mut snapshots: Vec<CursorPaint> = Vec::new();
                        let _ = cursors_weak.update(cx, |this, cx| {
                            let active_id = this.cursor_drag.as_ref().map(|d| d.cursor.read(cx).id);
                            let lp = this.line_plot.read(cx);
                            for cursor in &this.cursors {
                                let c = cursor.read(cx);
                                let mut markers: Vec<(f64, f64, Hsla)> = Vec::new();
                                for trace in lp.traces() {
                                    let cfg = trace.read(cx);
                                    if !cfg.visible {
                                        continue;
                                    }
                                    let v_start =
                                        lp.trace_value_at(trace.entity_id(), c.t_start, cx);
                                    let v_end = lp.trace_value_at(trace.entity_id(), c.t_end, cx);
                                    if let (Some(vs), Some(ve)) = (v_start, v_end) {
                                        // Pre-normalize to [0,1] against the
                                        // trace's axis so paint can map with a
                                        // single fixed 0..1 Y range.
                                        let (ns, ne) = match &view {
                                            Some(v) => {
                                                let b = v.axis_bounds(cfg.axis_index);
                                                let h = (b.max_y - b.min_y).max(1e-12);
                                                ((vs - b.min_y) / h, (ve - b.min_y) / h)
                                            }
                                            None => (vs, ve),
                                        };
                                        markers.push((ns, ne, cfg.color));
                                    }
                                }
                                snapshots.push(CursorPaint {
                                    t_start: c.t_start,
                                    t_end: c.t_end,
                                    active: Some(c.id) == active_id,
                                    trace_markers: markers,
                                });
                            }
                        });
                        (bounds, view, snapshots)
                    },
                    move |_, (bounds, view, cursors), window, cx| {
                        if let Some(view) = view {
                            // Markers carry normalized [0,1] Y; lines use X only.
                            paint_cursors(bounds, &view, &cursors, window, cx);
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        let mut out = None;
                        let _ = hover_weak.update(cx, |this, cx| {
                            let Some(pointer) = this.hover else {
                                return;
                            };
                            let Some(view) = this.line_plot.read(cx).effective_view(cx) else {
                                return;
                            };
                            // Same lookup as the readout box, pre-normalized
                            // per axis the way `CursorPaint` markers are.
                            let markers = this
                                .hover_samples(cx)
                                .map(|(_, samples)| {
                                    samples
                                        .into_iter()
                                        .map(|s| {
                                            let b = view.axis_bounds(s.axis_index);
                                            let h = (b.max_y - b.min_y).max(1e-12);
                                            (s.ts.0 as f64, (s.value - b.min_y) / h, s.color)
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            out = Some((
                                view,
                                HoverPaint {
                                    pointer_x: pointer.x,
                                    markers,
                                },
                            ));
                        });
                        (bounds, out)
                    },
                    move |_, (bounds, data), window, cx| {
                        if let Some((view, hover)) = data {
                            paint_hover_crosshair(bounds, &view, hover.pointer_x, window, cx);
                            paint_hover_markers(bounds, &view, &hover.markers, window);
                        }
                    },
                )
                .absolute()
                .inset_0(),
            );

        // Measurement panels: a native div tree positioned on top of
        // the cursor overlays. In Track mode this is one mini panel per
        // cursor; in Pinned mode it's a single consolidated panel.
        let panels = measurement_panel::render_panels(self, cx);
        for panel in panels {
            // Panel origins are plot-area-local; the outer container's
            // origin is offset by (left_margin, PADDING).
            let left = panel.origin.x + chrome_left;
            let top = panel.origin.y + px(PADDING);
            inner = inner.child(div().absolute().left(left).top(top).child(panel.element));
        }

        if let Some(readout) = self.hover_readout(cx) {
            inner = inner.child(readout);
        }

        // Event-flag popovers: a pinned one wins over the transient hover one.
        if let Some(popover) = self.event_pinned_popover(cx) {
            inner = inner.child(popover);
        } else if let Some(popover) = self.event_hover_popover(cx) {
            inner = inner.child(popover);
        }

        root = root.child(inner);

        if show_legend {
            root = root.child(crate::views::plot_common::plot_legend(
                &trace_entities,
                self.line_plot.clone(),
                chrome_left,
                |trace| (trace.label.clone(), trace.color, trace.visible),
                |trace| trace.visible = !trace.visible,
                cx,
            ));
        }

        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x_ticks_terminates_on_sub_microsecond_window() {
        // Width under 5us used to truncate the step to zero and loop forever.
        let view = PlotBounds::new(1_000.2, 0.0, 1_003.8, 1.0);
        let ticks: Vec<i64> = x_ticks(&view, 5, 1_000.2).take(2_000).collect();
        assert!(!ticks.is_empty());
        assert!(ticks.len() < 2_000);
        for pair in ticks.windows(2) {
            assert!(pair[1] > pair[0]);
        }
    }

    #[test]
    fn x_ticks_zero_width_range_yields_position_once() {
        let view = PlotBounds::new(5_000.0, 0.0, 5_000.0, 1.0);
        let ticks: Vec<i64> = x_ticks(&view, 5, 5_000.0).take(10).collect();
        assert_eq!(ticks, vec![5_000]);
    }

    #[test]
    fn x_ticks_snap_to_anchor_multiples() {
        let anchor = 1_000_003.0;
        let view = PlotBounds::new(anchor, 0.0, 10_000_000.0, 1.0);
        let ticks: Vec<i64> = x_ticks(&view, 5, anchor).collect();
        assert!(ticks.len() >= 2);
        let step = ticks[1] - ticks[0];
        assert!(step > 0);
        for &tick in &ticks {
            assert_eq!((tick - anchor as i64) % step, 0);
            assert!(tick >= view.min_x as i64 && tick <= view.max_x as i64);
        }
    }

    #[test]
    fn value_ticks_uses_nice_step() {
        let ticks: Vec<f64> = value_ticks(0.0, 1234.0, 5).collect();
        assert_eq!(ticks, vec![0.0, 250.0, 500.0, 750.0, 1000.0]);
    }

    #[test]
    fn value_ticks_zero_crossing_anchors_at_zero() {
        let ticks: Vec<f64> = value_ticks(-1.0, 1.0, 5).collect();
        assert!(ticks.contains(&0.0));
        assert_eq!(ticks.first(), Some(&-1.0));
        assert_eq!(ticks.last(), Some(&1.0));
    }

    #[test]
    fn value_ticks_tiny_fractional_range() {
        let (min, max) = (0.001, 0.002);
        let ticks: Vec<f64> = value_ticks(min, max, 5).collect();
        assert!(ticks.len() >= 5);
        let step = ticks[1] - ticks[0];
        assert!((step - 0.000_2).abs() < 1e-12);
        for pair in ticks.windows(2) {
            assert!((pair[1] - pair[0] - step).abs() < 1e-12);
        }
        for &tick in &ticks {
            assert!(tick >= min - 1e-12 && tick <= max + step * 0.02);
        }
    }

    #[test]
    fn value_ticks_empty_on_degenerate_range() {
        assert_eq!(value_ticks(1.0, 1.0, 5).count(), 0);
        assert_eq!(value_ticks(2.0, 1.0, 5).count(), 0);
    }
}
