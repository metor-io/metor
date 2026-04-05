use std::sync::Arc;

use gpui::{
    Bounds, Context, Hsla, IntoElement, MouseButton, PathBuilder, Pixels, Point, SharedString,
    Styled, TextRun, Window, canvas, div, point, prelude::*, px,
};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, Timestamp};

use crate::inspectable::{FieldId, Inspectable, InspectionField, InspectionValue, ListItem};
use crate::offset_parse::TimeRangeBehavior;
use crate::wait_for_component;

mod bounds;
pub use bounds::*;

/// Generate Y-axis tick positions within the visible bounds (sorted ascending).
/// When the range spans 0, ticks are anchored at 0 and extend outward.
fn y_ticks(view: &PlotBounds, target_count: usize) -> impl Iterator<Item = f64> {
    let step = pretty_round(view.height() / target_count as f64);
    let (start, end) = if !step.is_normal() || step <= 0.0 {
        (0.0, -1.0) // empty range — iterator yields nothing
    } else if view.min_y <= 0.0 && view.max_y >= 0.0 {
        // Anchor at 0: find the lowest tick by stepping down from 0
        let neg_steps = (-view.min_y / step).floor() as i64;
        let start = -step * neg_steps as f64;
        (start, view.max_y)
    } else {
        let start = (view.min_y / step).ceil() * step;
        (start, view.max_y + step * 0.01)
    };

    let mut v = start;
    std::iter::from_fn(move || {
        if v <= end {
            let tick = v;
            v += step;
            Some(tick)
        } else {
            None
        }
    })
}

/// Copied from metor-ui's `DurationExt::segment_round`.
/// Snaps a duration to a "nice" human-readable time interval.
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

/// Generate X-axis tick positions (timestamps in microseconds) within the visible bounds.
/// `data_start` is the absolute timestamp of the data origin so ticks align to round
/// offsets from it (e.g. 0 s, 5 s, 10 s, …).
fn x_ticks(view: &PlotBounds, target_count: usize, data_start: f64) -> impl Iterator<Item = i64> {
    let range = view.width();
    let target = (target_count as f64).max(1.0);
    let raw_step_us = range / target;
    let step_dur = segment_round(hifitime::Duration::from_microseconds(raw_step_us));
    let step_i = (step_dur.total_nanoseconds() / 1_000) as i64;

    let t_min = view.min_x as i64;
    let t_max = view.max_x as i64;
    let ds = data_start as i64;
    let valid = range > 0.0 && step_i > 0;

    // Align ticks to multiples of step relative to data_start
    let offset_from_start = t_min - ds;
    let aligned = if valid {
        ds + (offset_from_start / step_i) * step_i
    } else {
        t_min
    };
    let start = if aligned < t_min {
        aligned + step_i
    } else {
        aligned
    };
    let end = if valid { t_max } else { t_min };

    let mut t = start;
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        if t <= end {
            let tick = t;
            t += step_i;
            Some(tick)
        } else if !valid && !done {
            done = true;
            Some(t_min)
        } else {
            None
        }
    })
}

/// Format a timestamp as a human-readable time label relative to a reference point.
/// Uses hifitime's Duration display for consistent formatting with metor-ui
/// (e.g. "0", "30 s", "1 min", "1 min 30 s").
fn format_time_label(t_us: i64, ref_us: i64) -> String {
    let offset_us = t_us - ref_us;
    if offset_us == 0 {
        return "0".to_string();
    }
    let dur = hifitime::Duration::from_microseconds(offset_us as f64);
    format!("{}", dur)
}

fn format_value_label(v: f64) -> String {
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

const Y_LABEL_WIDTH: f32 = 50.0;
const X_LABEL_HEIGHT: f32 = 20.0;
const PADDING: f32 = 8.0;
const LABEL_FONT_SIZE: f32 = 11.0;

fn plot_area(outer: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: point(
            outer.origin.x + px(Y_LABEL_WIDTH + PADDING),
            outer.origin.y + px(PADDING),
        ),
        size: gpui::Size {
            width: (outer.size.width - px(Y_LABEL_WIDTH + PADDING * 2.0)).max(px(1.0)),
            height: (outer.size.height - px(X_LABEL_HEIGHT + PADDING * 2.0)).max(px(1.0)),
        },
    }
}

pub fn compute_y_bounds(component: &Component, indexes: &[usize]) -> Option<(f64, f64)> {
    expand_y_bounds(component, indexes, None, None)
}

/// Incrementally expand y bounds by scanning only data newer than `since`.
/// If `existing` bounds and a `since` timestamp are provided, only new data is
/// scanned and the bounds are expanded (never shrunk). When either is `None` a
/// full scan is performed. Considers all element `indexes`.
pub fn expand_y_bounds(
    component: &Component,
    indexes: &[usize],
    existing: Option<(f64, f64)>,
    since: Option<Timestamp>,
) -> Option<(f64, f64)> {
    let (mut min_y, mut max_y) = existing.unwrap_or((f64::INFINITY, f64::NEG_INFINITY));
    let mut any = existing.is_some();

    let mut update = |cv: &metor_proto::types::ComponentView| {
        for &idx in indexes {
            if let Some(ev) = cv.get(idx) {
                let v = ev.as_f64();
                min_y = min_y.min(v);
                max_y = max_y.max(v);
                any = true;
            }
        }
    };

    match since {
        Some(since_ts) => {
            // Incremental: only scan from since_ts onward
            let range = since_ts..Timestamp(i64::MAX);
            if let Some(slice) = component.time_series.get_range(range) {
                let schema = &component.schema;
                for node_slice in slice.as_iter() {
                    for (_ts, cv) in node_slice.iter_values(schema) {
                        update(&cv);
                    }
                }
            }
        }
        None => {
            // Full scan
            let element_size = component.schema.size();
            for node in component.time_series.list.iter() {
                let data = node.data.data();
                let count = node.timestamps().len();
                for i in 0..count {
                    let start = i * element_size;
                    if let Some(buf) = data.get(start..start + element_size) {
                        if let Ok((_size, view)) = component.schema.parse_value(buf) {
                            update(&view);
                        }
                    }
                }
            }
        }
    }

    any.then_some((min_y, max_y))
}

fn paint_plot<'a>(
    outer_bounds: Bounds<Pixels>,
    traces: impl Iterator<Item = &'a ResolvedTrace>,
    view: PlotBounds,
    data_start: f64,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let traces: Vec<_> = traces.collect();
    let pb = plot_area(outer_bounds);

    let theme = &crate::theme::DARK;
    let label_font_size = px(LABEL_FONT_SIZE);
    let text_style = window.text_style();
    let font = text_style.font();

    let make_run = |text: &str| TextRun {
        len: text.len(),
        font: font.clone(),
        color: theme.text_secondary,
        background_color: None,
        underline: None,
        strikethrough: None,
    };

    // Y axis: grid lines + labels in a single pass (no clipping needed)
    for tick in y_ticks(&view, 5) {
        let y = view.to_screen(pb, view.min_x, tick).y;
        if y < pb.origin.y || y > pb.origin.y + pb.size.height {
            continue;
        }
        // Grid line
        let mut grid = PathBuilder::stroke(px(0.5));
        grid.move_to(point(pb.origin.x, y));
        grid.line_to(point(pb.origin.x + pb.size.width, y));
        if let Ok(path) = grid.build() {
            window.paint_path(path, theme.grid_color);
        }
        // Label
        let text = format_value_label(tick);
        let run = make_run(&text);
        let shaped = window.text_system().shape_line(
            SharedString::from(text),
            label_font_size,
            &[run],
            None,
        );
        let origin = point(
            pb.origin.x - shaped.width - px(4.0),
            y - label_font_size / 2.0,
        );
        let _ = shaped.paint(origin, label_font_size, window, cx);
    }

    // X axis: grid lines + labels in a single pass (no clipping needed)
    let t_min_i = data_start as i64;
    for tick in x_ticks(&view, 5, data_start) {
        let x = view.to_screen(pb, tick as f64, view.min_y).x;
        if x < pb.origin.x || x > pb.origin.x + pb.size.width {
            continue;
        }
        // Grid line
        let mut grid = PathBuilder::stroke(px(0.5));
        grid.move_to(point(x, pb.origin.y));
        grid.line_to(point(x, pb.origin.y + pb.size.height));
        if let Ok(path) = grid.build() {
            window.paint_path(path, theme.grid_color);
        }
        // Label
        let text = format_time_label(tick, t_min_i);
        let run = make_run(&text);
        let shaped = window.text_system().shape_line(
            SharedString::from(text),
            label_font_size,
            &[run],
            None,
        );
        let origin = point(
            x - shaped.width / 2.0,
            pb.origin.y + pb.size.height + px(4.0),
        );
        let _ = shaped.paint(origin, label_font_size, window, cx);
    }

    // Zero line (bounded to plot area, no clipping needed)
    if view.min_y < 0.0 && view.max_y > 0.0 {
        let zero_y = view.to_screen(pb, view.min_x, 0.0).y;
        let mut zp = PathBuilder::stroke(px(1.0));
        zp.move_to(point(pb.origin.x, zero_y));
        zp.line_to(point(pb.origin.x + pb.size.width, zero_y));
        if let Ok(path) = zp.build() {
            window.paint_path(path, theme.zero_line_color);
        }
    }

    let mut axes = PathBuilder::stroke(px(1.0));
    axes.move_to(point(pb.origin.x, pb.origin.y));
    axes.line_to(point(pb.origin.x, pb.origin.y + pb.size.height));
    axes.line_to(point(
        pb.origin.x + pb.size.width,
        pb.origin.y + pb.size.height,
    ));
    if let Ok(path) = axes.build() {
        window.paint_path(path, theme.axis_color);
    }

    // Data traces — clipped to plot area
    window.with_content_mask(Some(gpui::ContentMask { bounds: pb }), |window| {
        for rt in &traces {
            if !rt.trace.visible {
                continue;
            }
            paint_trace(
                pb,
                &rt.component,
                &view,
                &rt.trace,
                window,
            );
        }
    });

}

fn paint_trace(
    screen_bounds: Bounds<Pixels>,
    component: &Component,
    view: &PlotBounds,
    trace: &Trace,
    window: &mut Window,
) {
    match trace.style {
        PlotStyle::Line => {
            paint_data_line(screen_bounds, component, view, trace.color, px(1.5), trace.element_index, window);
        }
        PlotStyle::Scatter => {
            paint_scatter(screen_bounds, component, view, trace.color, trace.element_index, window);
        }
        PlotStyle::Bar => {
            paint_bars(screen_bounds, component, view, trace.color, trace.element_index, window);
        }
    }
}

/// Decimation state that tracks min/max per pixel-width bucket.
/// Call `push` for each point in chronological order, then `flush` at the end.
struct Decimator {
    bucket_width: f64,
    bucket_start: f64,
    min_pt: Option<(f64, f64)>,
    max_pt: Option<(f64, f64)>,
    output: Vec<(f64, f64)>,
}

impl Decimator {
    fn new(t_min: f64, t_max: f64, pixel_budget: usize) -> Self {
        let range = t_max - t_min;
        let bucket_count = (pixel_budget / 2).max(1);
        Self {
            bucket_width: if range > 0.0 { range / bucket_count as f64 } else { f64::MAX },
            bucket_start: t_min,
            min_pt: None,
            max_pt: None,
            output: Vec::with_capacity(pixel_budget),
        }
    }

    fn push(&mut self, t: f64, v: f64) {
        // Flush completed buckets
        while t >= self.bucket_start + self.bucket_width {
            self.emit_bucket();
            self.bucket_start += self.bucket_width;
        }
        match self.min_pt {
            None => { self.min_pt = Some((t, v)); self.max_pt = Some((t, v)); }
            Some(cur) => {
                if v < cur.1 { self.min_pt = Some((t, v)); }
                if v > self.max_pt.unwrap().1 { self.max_pt = Some((t, v)); }
            }
        }
    }

    fn flush(mut self) -> Vec<(f64, f64)> {
        self.emit_bucket();
        self.output
    }

    fn emit_bucket(&mut self) {
        if let (Some(mn), Some(mx)) = (self.min_pt.take(), self.max_pt.take()) {
            if mn.0 == mx.0 {
                self.output.push(mn);
            } else if mn.0 <= mx.0 {
                self.output.push(mn);
                self.output.push(mx);
            } else {
                self.output.push(mx);
                self.output.push(mn);
            }
        }
    }
}

fn paint_scatter(
    screen_bounds: Bounds<Pixels>,
    component: &Component,
    view: &PlotBounds,
    color: Hsla,
    element_index: usize,
    window: &mut Window,
) {
    let start_ts = Timestamp(view.min_x as i64);
    let end_ts = Timestamp(view.max_x as i64);
    let Some(slice) = component.time_series.get_range(start_ts..end_ts) else {
        return;
    };
    let schema = &component.schema;
    let radius = px(3.0);
    let mut last_bucket: Option<i32> = None;

    // Order doesn't matter for scatter — iterate forward, skip same-pixel points
    for node_slice in slice.as_iter() {
        for (ts, cv) in node_slice.iter_values(schema) {
            let v = match cv.get(element_index) {
                Some(ev) => ev.as_f64(),
                None => continue,
            };
            let pt = view.to_screen(screen_bounds, ts.0 as f64, v);
            let bucket = f32::from(pt.x) as i32;
            if last_bucket == Some(bucket) {
                continue;
            }
            last_bucket = Some(bucket);

            let mut path = PathBuilder::fill();
            path.move_to(point(pt.x, pt.y - radius));
            path.line_to(point(pt.x + radius, pt.y));
            path.line_to(point(pt.x, pt.y + radius));
            path.line_to(point(pt.x - radius, pt.y));
            path.line_to(point(pt.x, pt.y - radius));
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        }
    }
}

fn paint_bars(
    screen_bounds: Bounds<Pixels>,
    component: &Component,
    view: &PlotBounds,
    color: Hsla,
    element_index: usize,
    window: &mut Window,
) {
    let start_ts = Timestamp(view.min_x as i64);
    let end_ts = Timestamp(view.max_x as i64);
    let Some(slice) = component.time_series.get_range(start_ts..end_ts) else {
        return;
    };
    let schema = &component.schema;
    let node_slices: Vec<_> = slice.as_iter().collect();
    let pixel_budget = f32::from(screen_bounds.size.width) as usize;

    let mut decimator = Decimator::new(view.min_x, view.max_x, pixel_budget);
    for node_slice in node_slices.iter().rev() {
        for (ts, cv) in node_slice.iter_values(schema) {
            if let Some(ev) = cv.get(element_index) {
                decimator.push(ts.0 as f64, ev.as_f64());
            }
        }
    }
    let points = decimator.flush();
    if points.is_empty() {
        return;
    }

    let max_bar_width = px(20.0);
    let bar_half =
        (screen_bounds.size.width / (points.len() as f32 * 2.0)).min(max_bar_width / 2.0);

    let baseline = if view.min_y <= 0.0 && view.max_y >= 0.0 {
        0.0
    } else {
        view.min_y
    };
    let baseline_y = view.to_screen(screen_bounds, view.min_x, baseline).y;

    for (t, v) in &points {
        let top = view.to_screen(screen_bounds, *t, *v);
        let mut path = PathBuilder::fill();
        let left = top.x - bar_half;
        let right = top.x + bar_half;
        let (top_y, bot_y) = if *v >= baseline {
            (top.y, baseline_y)
        } else {
            (baseline_y, top.y)
        };
        path.move_to(point(left, top_y));
        path.line_to(point(right, top_y));
        path.line_to(point(right, bot_y));
        path.line_to(point(left, bot_y));
        path.line_to(point(left, top_y));
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }
}

/// Draw a time series data line within the given screen bounds.
/// This is the core drawing primitive used by both the full plot and sparklines.
/// `element_index` selects which element of a multi-element component to plot
/// (e.g. index 0, 1, 2 for a Vec3). Does not clip — caller should wrap in
/// `with_content_mask` if needed.
pub fn paint_data_line(
    screen_bounds: Bounds<Pixels>,
    component: &Component,
    view: &PlotBounds,
    color: Hsla,
    stroke_width: Pixels,
    element_index: usize,
    window: &mut Window,
) {
    let start_ts = Timestamp(view.min_x as i64);
    let end_ts = Timestamp(view.max_x as i64);
    let Some(slice) = component.time_series.get_range(start_ts..end_ts) else {
        return;
    };
    let schema = &component.schema;
    let node_slices: Vec<_> = slice.as_iter().collect();
    let pixel_budget = f32::from(screen_bounds.size.width) as usize;

    let mut decimator = Decimator::new(view.min_x, view.max_x, pixel_budget * 2);
    for node_slice in node_slices.iter().rev() {
        for (ts, cv) in node_slice.iter_values(schema) {
            if let Some(ev) = cv.get(element_index) {
                decimator.push(ts.0 as f64, ev.as_f64());
            }
        }
    }
    let points = decimator.flush();

    if points.is_empty() {
        return;
    }

    let mut path = PathBuilder::stroke(stroke_width);
    for (i, (t, v)) in points.iter().enumerate() {
        let screen_pt = view.to_screen(screen_bounds, *t, *v);
        if i == 0 {
            path.move_to(screen_pt);
        } else {
            path.line_to(screen_pt);
        }
    }

    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// How a trace is drawn on the plot.
#[derive(Clone, Copy, Default, PartialEq)]
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

/// A single trace on a time series plot: one element index from one component.
#[derive(Clone)]
pub struct Trace {
    pub component_id: ComponentId,
    pub element_index: usize,
    pub color: Hsla,
    pub style: PlotStyle,
    pub visible: bool,
    pub label: SharedString,
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
        }
    }
}

#[derive(Clone)]
struct ResolvedTrace {
    trace: Trace,
    component: Component,
    y_bounds: Option<(f64, f64)>,
    last_scan_ts: Option<Timestamp>,
}

/// Interactive time-series plot with multi-trace support, pan, and zoom.
pub struct TimeSeriesPlot {
    db: Arc<DB>,
    traces: Vec<Option<ResolvedTrace>>,
    view: Option<PlotBounds>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotBounds>,
    last_plot_area: Option<Bounds<Pixels>>,
    x_range: TimeRangeBehavior,
    y_min_override: Option<f64>,
    y_max_override: Option<f64>,
    _tasks: Vec<gpui::Task<()>>,
}

impl TimeSeriesPlot {
    pub fn new(db: Arc<DB>, traces: Vec<Trace>, cx: &mut Context<Self>) -> Self {
        let tasks = traces
            .iter()
            .enumerate()
            .map(|(i, trace)| Self::spawn_trace(db.clone(), i, trace, cx))
            .collect();
        let trace_slots = (0..traces.len()).map(|_| None).collect();
        Self {
            db,
            traces: trace_slots,
            view: None,
            drag_start: None,
            drag_start_view: None,
            last_plot_area: None,
            x_range: TimeRangeBehavior::default(),
            y_min_override: None,
            y_max_override: None,
            _tasks: tasks,
        }
    }

    /// Convenience: create a plot for a single component with the given element
    /// indexes, auto-assigning colors from the theme palette.
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
        let theme = &crate::theme::DARK;
        let elem_names = crate::trace_picker::element_names_for_component(&db, component_id);
        let comp_name = db
            .with_state(|s| {
                s.get_component_metadata(component_id)
                    .map(|m| m.name.clone())
            })
            .unwrap_or_default();
        let traces = indexes
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
                }
            })
            .collect();
        Self::new(db, traces, cx)
    }

    pub fn set_traces(&mut self, traces: Vec<Trace>, cx: &mut Context<Self>) {
        self._tasks = traces
            .iter()
            .enumerate()
            .map(|(i, trace)| Self::spawn_trace(self.db.clone(), i, trace, cx))
            .collect();
        self.traces = (0..traces.len()).map(|_| None).collect();
        self.view = None;
        cx.notify();
    }

    fn spawn_trace(
        db: Arc<DB>,
        trace_idx: usize,
        trace: &Trace,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        let trace = trace.clone();
        cx.spawn(async move |this, cx| {
            let component = wait_for_component(&db, trace.component_id).await;

            let result = this.update(cx, |this, cx| {
                this.traces[trace_idx] = Some(ResolvedTrace {
                    trace: trace.clone(),
                    component: component.clone(),
                    y_bounds: None,
                    last_scan_ts: None,
                });
                cx.notify();
            });
            if result.is_err() {
                return;
            }

            loop {
                let result = this.update(cx, |this, cx| {
                    if let Some(resolved) = &mut this.traces[trace_idx] {
                        let latest_ts = resolved
                            .component
                            .time_series
                            .latest()
                            .map(|n| n.timestamp());
                        resolved.y_bounds = expand_y_bounds(
                            &resolved.component,
                            &[resolved.trace.element_index],
                            resolved.y_bounds,
                            resolved.last_scan_ts,
                        );
                        resolved.last_scan_ts = latest_ts;
                    }
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                component.time_series.wait().await;
            }
        })
    }

    fn resolved_traces(&self) -> impl Iterator<Item = &ResolvedTrace> {
        self.traces.iter().flatten()
    }

    fn merged_y_bounds(&self) -> Option<(f64, f64)> {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut any = false;
        for r in self.resolved_traces() {
            if let Some((lo, hi)) = r.y_bounds {
                min = min.min(lo);
                max = max.max(hi);
                any = true;
            }
        }
        any.then_some((min, max))
    }

    fn time_range(&self) -> Option<(f64, f64)> {
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut any = false;
        for r in self.resolved_traces() {
            if let Some(s) = r.component.time_series.start_timestamp() {
                start = start.min(s.0 as f64);
                any = true;
            }
            if let Some(l) = r.component.time_series.latest() {
                end = end.max(l.timestamp().0 as f64);
                any = true;
            }
        }
        if any && start < end {
            Some((start, end))
        } else {
            None
        }
    }

    fn effective_y_bounds(&self) -> (f64, f64) {
        let (auto_min, auto_max) = self.merged_y_bounds().unwrap_or((0.0, 1.0));
        (
            self.y_min_override.unwrap_or(auto_min),
            self.y_max_override.unwrap_or(auto_max),
        )
    }

    fn current_view(&self) -> Option<PlotBounds> {
        self.view.or_else(|| {
            let (data_start, data_end) = self.time_range()?;
            let range = self.x_range.calculate_range(
                Timestamp(data_start as i64),
                Timestamp(data_end as i64),
            );
            let (min_y, max_y) = self.effective_y_bounds();
            Some(PlotBounds::new(range.start.0 as f64, min_y, range.end.0 as f64, max_y).normalize())
        })
    }
}

impl Render for TimeSeriesPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.current_view();
        let data_start = self.time_range().map(|(s, _)| s).unwrap_or(0.0);
        let traces = self.traces.clone();
        let show_legend = self.resolved_traces().count() >= 2;
        let theme = &crate::theme::DARK;

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_secondary)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                            if event.click_count == 2 {
                                this.view = None;
                                this.x_range = TimeRangeBehavior::default();
                                this.y_min_override = None;
                                this.y_max_override = None;
                                cx.notify();
                            } else {
                                this.drag_start = Some(event.position);
                                this.drag_start_view = this.current_view();
                            }
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _event: &gpui::MouseUpEvent, _window, _cx| {
                            this.drag_start = None;
                            this.drag_start_view = None;
                        }),
                    )
                    .on_mouse_move(
                        cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                            if !event.dragging() {
                                return;
                            }
                            let (Some(start), Some(start_view), Some(pa)) =
                                (this.drag_start, this.drag_start_view, this.last_plot_area)
                            else {
                                return;
                            };

                            let dx = event.position.x - start.x;
                            let dy = event.position.y - start.y;
                            let (nx, ny) = start_view.screen_delta_to_norm(pa, dx, dy);
                            this.view = Some(start_view.offset_by_norm(-nx, ny));
                            cx.notify();
                        }),
                    )
                    .on_scroll_wheel(
                        cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                            let (Some(view), Some(pa)) =
                                (this.current_view(), this.last_plot_area)
                            else {
                                return;
                            };

                            let delta = event.delta.pixel_delta(px(20.0));
                            let zoom_amount = f32::from(-delta.y) as f64 / 200.0;
                            let factor = (1.0_f64 + zoom_amount).clamp(0.5, 2.0);

                            let (ax, ay) = view.screen_anchor(pa, event.position);
                            this.view = Some(view.zoom_at(factor, ax, 1.0 - ay));
                            cx.notify();
                        }),
                    )
                    .child(
                        canvas(
                            {
                                let this = cx.entity().downgrade();
                                move |bounds, _window, cx| {
                                    let pa = plot_area(bounds);
                                    let _ = this.update(cx, |this, _cx| {
                                        this.last_plot_area = Some(pa);
                                    });
                                    (bounds, view)
                                }
                            },
                            move |_, (bounds, view), window, cx| {
                                if let Some(view) = view {
                                    paint_plot(
                                        bounds,
                                        traces.iter().flatten(),
                                        view,
                                        data_start,
                                        window,
                                        cx,
                                    );
                                }
                            },
                        )
                        .size_full(),
                    ),
            );

        if show_legend {
            let mut legend_row = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_3()
                .px(px(Y_LABEL_WIDTH + PADDING))
                .py_1();

            for (i, rt) in self.traces.iter().enumerate() {
                let Some(rt) = rt else { continue };
                let visible = rt.trace.visible;
                let opacity = if visible { 1.0 } else { 0.3 };
                let color = Hsla { a: opacity, ..rt.trace.color };
                let text_color = Hsla { a: opacity, ..theme.text_secondary };

                legend_row = legend_row.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event: &gpui::MouseDownEvent, _window, cx| {
                                if let Some(Some(resolved)) = this.traces.get_mut(i) {
                                    resolved.trace.visible = !resolved.trace.visible;
                                    cx.notify();
                                }
                            }),
                        )
                        .child(
                            div()
                                .w(px(10.0))
                                .h(px(10.0))
                                .rounded(px(2.0))
                                .bg(color),
                        )
                        .child(
                            div()
                                .text_size(px(LABEL_FONT_SIZE))
                                .text_color(text_color)
                                .child(rt.trace.label.clone()),
                        ),
                );
            }

            root = root.child(legend_row);
        }

        root
    }
}

/// Field IDs for trace sub-fields: `trace_index * 100 + sub_field`.
const TRACE_SETTINGS_BASE: u32 = 100;
const TRACE_SUB_STYLE: u32 = 0;
const TRACE_SUB_COLOR: u32 = 1;
const TRACE_SUB_VISIBLE: u32 = 2;

impl Inspectable for TimeSeriesPlot {
    fn fields(&self) -> Vec<InspectionField> {
        let current_traces: Vec<(ComponentId, usize)> = self
            .resolved_traces()
            .map(|rt| (rt.trace.component_id, rt.trace.element_index))
            .collect();

        let mut fields = vec![InspectionField {
            label: "Traces".into(),
            field_id: FieldId(0),
            value: InspectionValue::Traces(current_traces),
        }];

        // Per-trace settings as a nested list
        let trace_items: Vec<ListItem> = self
            .traces
            .iter()
            .enumerate()
            .filter_map(|(i, rt)| {
                let rt = rt.as_ref()?;
                let base = TRACE_SETTINGS_BASE + i as u32 * 100;
                Some(ListItem {
                    label: rt.trace.label.clone(),
                    fields: vec![
                        InspectionField {
                            label: "Style".into(),
                            field_id: FieldId(base + TRACE_SUB_STYLE),
                            value: InspectionValue::Enum {
                                selected: rt.trace.style.label().to_string(),
                                options: PlotStyle::ALL
                                    .iter()
                                    .map(|s| s.label().to_string())
                                    .collect(),
                            },
                        },
                        InspectionField {
                            label: "Color".into(),
                            field_id: FieldId(base + TRACE_SUB_COLOR),
                            value: InspectionValue::Color(rt.trace.color),
                        },
                        InspectionField {
                            label: "Visible".into(),
                            field_id: FieldId(base + TRACE_SUB_VISIBLE),
                            value: InspectionValue::Bool(rt.trace.visible),
                        },
                    ],
                })
            })
            .collect();

        if !trace_items.is_empty() {
            fields.push(InspectionField {
                label: "Trace Settings".into(),
                field_id: FieldId(1),
                value: InspectionValue::List(trace_items),
            });
        }

        fields.push(InspectionField {
            label: "X Range".into(),
            field_id: FieldId(4),
            value: InspectionValue::String(self.x_range.to_string()),
        });

        let (min_y, max_y) = self.effective_y_bounds();
        fields.push(InspectionField {
            label: "Y Min".into(),
            field_id: FieldId(2),
            value: InspectionValue::F64(min_y),
        });
        fields.push(InspectionField {
            label: "Y Max".into(),
            field_id: FieldId(3),
            value: InspectionValue::F64(max_y),
        });

        fields
    }

    fn set_field(&mut self, field_id: FieldId, value: InspectionValue, cx: &mut Context<Self>) {
        match (field_id, value) {
            (FieldId(0), InspectionValue::Traces(selections)) => {
                let theme = &crate::theme::DARK;
                let new_traces: Vec<Trace> = selections
                    .into_iter()
                    .enumerate()
                    .map(|(i, (component_id, element_index))| {
                        let elem_names =
                            crate::trace_picker::element_names_for_component(&self.db, component_id);
                        let comp_name = self
                            .db
                            .with_state(|s| {
                                s.get_component_metadata(component_id)
                                    .map(|m| m.name.clone())
                            })
                            .unwrap_or_default();
                        let label = elem_names
                            .get(element_index)
                            .map(|n| format!("{}.{}", comp_name, n))
                            .unwrap_or_else(|| format!("{}[{}]", comp_name, element_index));
                        Trace {
                            component_id,
                            element_index,
                            color: theme.line_colors[i % theme.line_colors.len()],
                            style: PlotStyle::default(),
                            visible: true,
                            label: SharedString::from(label),
                        }
                    })
                    .collect();
                if !new_traces.is_empty() {
                    self.set_traces(new_traces, cx);
                }
            }
            (FieldId(2), InspectionValue::F64(v)) => {
                self.y_min_override = Some(v);
                self.view = None;
                cx.notify();
            }
            (FieldId(3), InspectionValue::F64(v)) => {
                self.y_max_override = Some(v);
                self.view = None;
                cx.notify();
            }
            (FieldId(4), InspectionValue::String(s)) => {
                if let Ok(behavior) = s.parse::<TimeRangeBehavior>() {
                    self.x_range = behavior;
                    self.view = None;
                    cx.notify();
                }
            }
            (FieldId(id), value) if id >= TRACE_SETTINGS_BASE => {
                let trace_idx = ((id - TRACE_SETTINGS_BASE) / 100) as usize;
                let sub_field = (id - TRACE_SETTINGS_BASE) % 100;
                if let Some(Some(resolved)) = self.traces.get_mut(trace_idx) {
                    match (sub_field, value) {
                        (TRACE_SUB_STYLE, InspectionValue::Enum { selected, .. }) => {
                            if let Some(style) = PlotStyle::parse(&selected) {
                                resolved.trace.style = style;
                            }
                        }
                        (TRACE_SUB_COLOR, InspectionValue::Color(c)) => {
                            resolved.trace.color = c;
                        }
                        (TRACE_SUB_VISIBLE, InspectionValue::Bool(b)) => {
                            resolved.trace.visible = b;
                        }
                        _ => {}
                    }
                    cx.notify();
                }
            }
            _ => {}
        }
    }
}
