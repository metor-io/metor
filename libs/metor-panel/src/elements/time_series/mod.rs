use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, Hsla, IntoElement, MouseButton, PathBuilder, Pixels, Point,
    RenderImage, SharedString, Styled, TextRun, Window, canvas, div, point, prelude::*, px,
};
use metor_db::time_series::TimeSeriesNodeSlice;
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, PrimType, Timestamp};

use crate::inspectable::{FieldId, Inspectable, InspectionField, InspectionValue, ListItem};
use crate::offset_parse::TimeRangeBehavior;
use crate::wait_for_component;

mod bounds;
pub use bounds::*;

mod gpu;

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
            // Snap near-zero values to exactly 0 to avoid displaying "-5.6e-17"
            let tick = if v.abs() < step * 1e-10 { 0.0 } else { v };
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

#[derive(Clone, Copy, PartialEq)]
enum AxisZone {
    Plot,
    XAxis,
    YAxis,
}

fn axis_zone(pos: Point<Pixels>, plot_area: Bounds<Pixels>) -> AxisZone {
    let below_plot = pos.y > plot_area.origin.y + plot_area.size.height;
    let left_of_plot = pos.x < plot_area.origin.x;
    if left_of_plot {
        AxisZone::YAxis
    } else if below_plot {
        AxisZone::XAxis
    } else {
        AxisZone::Plot
    }
}

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
    line_image: Option<Arc<RenderImage>>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let pb = plot_area(outer_bounds);

    let theme = crate::theme::theme(cx);
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

    // 1. Grid lines (behind everything)
    for tick in y_ticks(&view, 5) {
        let y = view.to_screen(pb, view.min_x, tick).y;
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
    let t_min_i = data_start as i64;
    for tick in x_ticks(&view, 5, data_start) {
        let x = view.to_screen(pb, tick as f64, view.min_y).x;
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

    // 2. Data traces — clipped to outer bounds so lines extend under axis areas.
    //    When the GPU path produced an image, blit it for the line traces and
    //    let the CPU path handle scatter/bar.
    window.with_content_mask(
        Some(gpui::ContentMask {
            bounds: outer_bounds,
        }),
        |window| {
            if let Some(img) = line_image {
                let _ = window.paint_image(pb, Corners::default(), img, 0, false);
            }
        },
    );

    // 3. Semi-transparent fills over axis label areas (partially occlude lines)
    let axis_bg = Hsla {
        a: 0.5,
        ..theme.bg_secondary
    };
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

    // 4. Labels on top of the fills
    for tick in y_ticks(&view, 5) {
        let y = view.to_screen(pb, view.min_x, tick).y;
        if y < pb.origin.y || y > pb.origin.y + pb.size.height {
            continue;
        }
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
    for tick in x_ticks(&view, 5, data_start) {
        let x = view.to_screen(pb, tick as f64, view.min_y).x;
        if x < pb.origin.x || x > pb.origin.x + pb.size.width {
            continue;
        }
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

    // 5. Zero line and axes on top
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
            paint_data_line(
                screen_bounds,
                component,
                view,
                trace.color,
                px(trace.stroke_width),
                trace.element_index,
                window,
            );
        }
        PlotStyle::Scatter => {
            paint_scatter(
                screen_bounds,
                component,
                view,
                trace.color,
                trace.element_index,
                window,
            );
        }
        PlotStyle::Bar => {
            paint_bars(
                screen_bounds,
                component,
                view,
                trace.color,
                trace.element_index,
                window,
            );
        }
    }
}

// ── Fast data access ────────────────────────────────────────────────

/// Trait for primitive types that can be read from a byte buffer and converted to f64.
trait PlotValue: zerocopy::FromBytes + Copy + Sized + 'static {
    fn to_f64(self) -> f64;
}

macro_rules! impl_plot_value {
    ($($ty:ty => $conv:expr),* $(,)?) => {
        $(impl PlotValue for $ty {
            #[inline(always)]
            fn to_f64(self) -> f64 { $conv(self) }
        })*
    };
}

impl_plot_value! {
    f64  => |x| x,
    f32  => |x: f32| x as f64,
    i8   => |x: i8| x as f64,
    i16  => |x: i16| x as f64,
    i32  => |x: i32| x as f64,
    i64  => |x: i64| x as f64,
    u8   => |x: u8| x as f64,
    u16  => |x: u16| x as f64,
    u32  => |x: u32| x as f64,
    u64  => |x: u64| x as f64,
}

/// Bool isn't `FromBytes` (not all bit patterns valid), so treat it as u8.
/// The dispatch macro maps `PrimType::Bool` to `u8` which reads the byte
/// and converts via `u8::to_f64` (0 or 1).

/// Read a single element value directly from a raw data buffer.
#[inline(always)]
fn read_value<T: PlotValue>(
    data: &[u8],
    sample_index: usize,
    elem_size: usize,
    elem_index: usize,
) -> Option<f64> {
    let offset = sample_index * elem_size + elem_index * size_of::<T>();
    let buf = data.get(offset..offset + size_of::<T>())?;
    T::read_from_bytes(buf).ok().map(|v| v.to_f64())
}

/// Compute per-node stride for two-level decimation.
/// Returns the step between sample indices within a node, or `None` to skip it entirely.
fn node_stride(node_len: usize, total: usize, pixel_budget: usize) -> Option<usize> {
    if node_len == 0 || total == 0 || pixel_budget == 0 {
        return None;
    }
    let node_budget = ((node_len as f64 / total as f64) * pixel_budget as f64).ceil() as usize;
    if node_budget == 0 {
        return None;
    }
    Some((node_len / node_budget).max(1))
}

// ── Query helpers ───────────────────────────────────────────────────

fn query_range(view: &PlotBounds, screen_width_px: f32) -> (Timestamp, Timestamp) {
    let margin_px = Y_LABEL_WIDTH + PADDING;
    if screen_width_px <= 0.0 {
        return (Timestamp(view.min_x as i64), Timestamp(view.max_x as i64));
    }
    let time_per_px = view.width() / screen_width_px as f64;
    let margin_time = (margin_px as f64 * time_per_px) as i64;
    (
        Timestamp(view.min_x as i64 - margin_time),
        Timestamp(view.max_x as i64 + margin_time),
    )
}

// ── Paint functions ─────────────────────────────────────────────────

fn paint_scatter(
    screen_bounds: Bounds<Pixels>,
    component: &Component,
    view: &PlotBounds,
    color: Hsla,
    element_index: usize,
    window: &mut Window,
) {
    let (start_ts, end_ts) = query_range(view, f32::from(screen_bounds.size.width));
    let Some(slice) = component.time_series.get_range(start_ts..end_ts) else {
        return;
    };
    let node_slices: Vec<_> = slice.as_iter().collect();
    let pixel_budget = f32::from(screen_bounds.size.width) as usize;
    let total: usize = node_slices.iter().map(|ns| ns.timestamps().len()).sum();
    let xf = view.screen_transform(screen_bounds);
    let elem_size = component.schema.size();

    fn inner<T: PlotValue>(
        node_slices: &[TimeSeriesNodeSlice],
        total: usize,
        pixel_budget: usize,
        elem_size: usize,
        elem_index: usize,
        xf: &ScreenTransform,
        path: &mut PathBuilder,
        any: &mut bool,
    ) {
        let radius = px(3.0);
        for ns in node_slices.iter() {
            let Some(stride) = node_stride(ns.timestamps().len(), total, pixel_budget) else {
                continue;
            };
            let timestamps = ns.timestamps();
            let data = ns.data();
            let mut i = 0;
            while i < timestamps.len() {
                if let Some(v) = read_value::<T>(data, i, elem_size, elem_index) {
                    let pt = xf.apply(timestamps[i].0 as f64, v);
                    path.move_to(point(pt.x, pt.y - radius));
                    path.line_to(point(pt.x + radius, pt.y));
                    path.line_to(point(pt.x, pt.y + radius));
                    path.line_to(point(pt.x - radius, pt.y));
                    path.line_to(point(pt.x, pt.y - radius));
                    *any = true;
                }
                i += stride;
            }
        }
    }

    let mut path = PathBuilder::fill();
    let mut any = false;
    match component.schema.prim_type {
        PrimType::F64 => inner::<f64>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
        PrimType::F32 => inner::<f32>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
        PrimType::I64 => inner::<i64>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
        PrimType::I32 => inner::<i32>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
        PrimType::I16 => inner::<i16>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
        PrimType::I8 => inner::<i8>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
        PrimType::U64 => inner::<u64>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
        PrimType::U32 => inner::<u32>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
        PrimType::U16 => inner::<u16>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
        PrimType::U8 | PrimType::Bool => inner::<u8>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut any,
        ),
    }

    if any {
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
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
    let (start_ts, end_ts) = query_range(view, f32::from(screen_bounds.size.width));
    let Some(slice) = component.time_series.get_range(start_ts..end_ts) else {
        return;
    };
    let node_slices: Vec<_> = slice.as_iter().collect();
    let pixel_budget = f32::from(screen_bounds.size.width) as usize;
    let total: usize = node_slices.iter().map(|ns| ns.timestamps().len()).sum();
    let xf = view.screen_transform(screen_bounds);
    let elem_size = component.schema.size();

    let baseline = if view.min_y <= 0.0 && view.max_y >= 0.0 {
        0.0
    } else {
        view.min_y
    };
    let baseline_y = xf.apply(view.min_x, baseline).y;
    let budget = pixel_budget.max(1);
    let max_bar_width = px(20.0);
    let bar_half = (screen_bounds.size.width / (budget as f32 * 2.0)).min(max_bar_width / 2.0);

    fn inner<T: PlotValue>(
        node_slices: &[TimeSeriesNodeSlice],
        total: usize,
        pixel_budget: usize,
        elem_size: usize,
        elem_index: usize,
        xf: &ScreenTransform,
        baseline: f64,
        baseline_y: Pixels,
        bar_half: Pixels,
        path: &mut PathBuilder,
        any: &mut bool,
    ) {
        for ns in node_slices.iter().rev() {
            let Some(stride) = node_stride(ns.timestamps().len(), total, pixel_budget) else {
                continue;
            };
            let timestamps = ns.timestamps();
            let data = ns.data();
            let mut i = 0;
            while i < timestamps.len() {
                if let Some(v) = read_value::<T>(data, i, elem_size, elem_index) {
                    let t = timestamps[i].0 as f64;
                    let top = xf.apply(t, v);
                    let left = top.x - bar_half;
                    let right = top.x + bar_half;
                    let (top_y, bot_y) = if v >= baseline {
                        (top.y, baseline_y)
                    } else {
                        (baseline_y, top.y)
                    };
                    path.move_to(point(left, top_y));
                    path.line_to(point(right, top_y));
                    path.line_to(point(right, bot_y));
                    path.line_to(point(left, bot_y));
                    path.line_to(point(left, top_y));
                    *any = true;
                }
                i += stride;
            }
        }
    }

    let mut path = PathBuilder::fill();
    let mut any = false;
    match component.schema.prim_type {
        PrimType::F64 => inner::<f64>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
        PrimType::F32 => inner::<f32>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
        PrimType::I64 => inner::<i64>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
        PrimType::I32 => inner::<i32>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
        PrimType::I16 => inner::<i16>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
        PrimType::I8 => inner::<i8>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
        PrimType::U64 => inner::<u64>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
        PrimType::U32 => inner::<u32>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
        PrimType::U16 => inner::<u16>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
        PrimType::U8 | PrimType::Bool => inner::<u8>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            baseline,
            baseline_y,
            bar_half,
            &mut path,
            &mut any,
        ),
    }

    if any {
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }
}

/// Draw a time series data line within the given screen bounds.
/// Reads values directly from the raw byte buffer, bypassing parse_value.
/// Uses two-level stride decimation: skips entire nodes when possible,
/// then samples within nodes at a proportional rate.
pub fn paint_data_line(
    screen_bounds: Bounds<Pixels>,
    component: &Component,
    view: &PlotBounds,
    color: Hsla,
    stroke_width: Pixels,
    element_index: usize,
    window: &mut Window,
) {
    let (start_ts, end_ts) = query_range(view, f32::from(screen_bounds.size.width));
    let Some(slice) = component.time_series.get_range(start_ts..end_ts) else {
        return;
    };
    let node_slices: Vec<_> = slice.as_iter().collect();
    let pixel_budget = f32::from(screen_bounds.size.width) as usize;
    let total: usize = node_slices.iter().map(|ns| ns.timestamps().len()).sum();
    let xf = view.screen_transform(screen_bounds);
    let elem_size = component.schema.size();

    fn inner<T: PlotValue>(
        node_slices: &[TimeSeriesNodeSlice],
        total: usize,
        pixel_budget: usize,
        elem_size: usize,
        elem_index: usize,
        xf: &ScreenTransform,
        path: &mut PathBuilder,
        first: &mut bool,
    ) {
        for ns in node_slices.iter().rev() {
            let Some(stride) = node_stride(ns.timestamps().len(), total, pixel_budget) else {
                continue;
            };
            let timestamps = ns.timestamps();
            let data = ns.data();
            let mut i = 0;
            while i < timestamps.len() {
                if let Some(v) = read_value::<T>(data, i, elem_size, elem_index) {
                    let pt = xf.apply(timestamps[i].0 as f64, v);
                    if *first {
                        path.move_to(pt);
                        *first = false;
                    } else {
                        path.line_to(pt);
                    }
                }
                i += stride;
            }
        }
    }

    let mut path = PathBuilder::stroke(stroke_width);
    let mut first = true;
    match component.schema.prim_type {
        PrimType::F64 => inner::<f64>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
        PrimType::F32 => inner::<f32>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
        PrimType::I64 => inner::<i64>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
        PrimType::I32 => inner::<i32>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
        PrimType::I16 => inner::<i16>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
        PrimType::I8 => inner::<i8>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
        PrimType::U64 => inner::<u64>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
        PrimType::U32 => inner::<u32>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
        PrimType::U16 => inner::<u16>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
        PrimType::U8 | PrimType::Bool => inner::<u8>(
            &node_slices,
            total,
            pixel_budget,
            elem_size,
            element_index,
            &xf,
            &mut path,
            &mut first,
        ),
    }

    if !first {
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
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
    pub stroke_width: f32,
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
    custom_title: Option<SharedString>,
    view: Option<PlotBounds>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotBounds>,
    drag_zone: AxisZone,
    last_plot_area: Option<Bounds<Pixels>>,
    x_range: TimeRangeBehavior,
    y_min_override: Option<f64>,
    y_max_override: Option<f64>,
    on_open_page: Option<OpenPageCallback>,
    gpu_line: Option<gpu::LineRenderer>,
    gpu_line_image: Option<Arc<RenderImage>>,
    gpu_dropped_images: Vec<Arc<RenderImage>>,
    _tasks: Vec<gpui::Task<()>>,
}

/// Callback type for when the plot wants to open an inspector page.
pub type OpenPageCallback =
    Arc<dyn Fn(crate::command_palette::PalettePage, &mut Window, &mut gpui::App) + 'static>;

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
            custom_title: None,
            view: None,
            drag_start: None,
            drag_start_view: None,
            drag_zone: AxisZone::Plot,
            last_plot_area: None,
            x_range: TimeRangeBehavior::default(),
            y_min_override: None,
            y_max_override: None,
            on_open_page: None,
            gpu_line: None,
            gpu_line_image: None,
            gpu_dropped_images: Vec::new(),
            _tasks: tasks,
        }
    }

    pub fn set_on_open_page(&mut self, cb: OpenPageCallback) {
        self.on_open_page = Some(cb);
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
        let theme = crate::theme::theme(cx);
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
                    stroke_width: 1.5,
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

    /// Returns the display title: custom title if set, otherwise derived from traces.
    ///
    /// Groups traces by component. If all elements are present, shows just the
    /// component name. Otherwise shows `component[x,y]`.
    pub fn title(&self) -> SharedString {
        if let Some(title) = &self.custom_title {
            return title.clone();
        }

        use std::collections::HashMap;
        let mut groups: HashMap<ComponentId, Vec<usize>> = HashMap::new();
        // Track insertion order so the title is deterministic.
        let mut order: Vec<ComponentId> = Vec::new();
        for rt in self.traces.iter().flatten() {
            let id = rt.trace.component_id;
            groups
                .entry(id)
                .or_insert_with(|| {
                    order.push(id);
                    Vec::new()
                })
                .push(rt.trace.element_index);
        }

        if order.is_empty() {
            return "Plot".into();
        }

        let parts: Vec<String> = order
            .iter()
            .map(|comp_id| {
                let indexes = &groups[comp_id];
                let all_elements =
                    crate::trace_picker::element_names_for_component(&self.db, *comp_id);
                let comp_name = self
                    .db
                    .with_state(|s| s.get_component_metadata(*comp_id).map(|m| m.name.clone()))
                    .unwrap_or_default();

                if indexes.len() == all_elements.len() {
                    comp_name
                } else {
                    let names: Vec<&str> = indexes
                        .iter()
                        .filter_map(|&i| all_elements.get(i).map(|s| s.as_str()))
                        .collect();
                    format!("{}[{}]", comp_name, names.join(","))
                }
            })
            .collect();

        SharedString::from(parts.join(", "))
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
            let range = self
                .x_range
                .calculate_range(Timestamp(data_start as i64), Timestamp(data_end as i64));
            let (min_y, max_y) = self.effective_y_bounds();
            Some(
                PlotBounds::new(range.start.0 as f64, min_y, range.end.0 as f64, max_y).normalize(),
            )
        })
    }

    fn trace_inspect_page(
        &self,
        trace_idx: usize,
        cx: &Context<Self>,
    ) -> crate::command_palette::PalettePage {
        use crate::inspectable::palette_page_for_list_item;

        let rt = self.traces[trace_idx].as_ref().unwrap();
        let base = TRACE_SETTINGS_BASE + trace_idx as u32 * 100;
        let fields = vec![
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
            InspectionField {
                label: "Width".into(),
                field_id: FieldId(base + TRACE_SUB_STROKE_WIDTH),
                value: InspectionValue::F64(rt.trace.stroke_width as f64),
            },
        ];
        palette_page_for_list_item(cx.entity().clone(), &fields, rt.trace.label.clone(), None)
    }
}

impl Render for TimeSeriesPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.current_view();
        let data_start = self.time_range().map(|(s, _)| s).unwrap_or(0.0);
        let traces = self.traces.clone();
        let show_legend = self.resolved_traces().count() >= 2;
        let theme = crate::theme::theme(cx);

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
                                let zone = this
                                    .last_plot_area
                                    .map(|pa| axis_zone(event.position, pa))
                                    .unwrap_or(AxisZone::Plot);
                                this.drag_start = Some(event.position);
                                this.drag_start_view = this.current_view();
                                this.drag_zone = zone;
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
                    .on_mouse_move(cx.listener(
                        |this, event: &gpui::MouseMoveEvent, _window, cx| {
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
                            this.view = Some(match this.drag_zone {
                                AxisZone::Plot => start_view.offset_by_norm(-nx, ny),
                                AxisZone::XAxis => start_view.offset_x(-nx),
                                AxisZone::YAxis => start_view.offset_y(ny),
                            });
                            cx.notify();
                        },
                    ))
                    .on_scroll_wheel(cx.listener(
                        |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                            let (Some(view), Some(pa)) = (this.current_view(), this.last_plot_area)
                            else {
                                return;
                            };

                            let delta = event.delta.pixel_delta(px(20.0));
                            let zoom_amount = f32::from(-delta.y) as f64 / 200.0;
                            let factor = (1.0_f64 + zoom_amount).clamp(0.5, 2.0);

                            let zone = axis_zone(event.position, pa);
                            let (ax, ay) = view.screen_anchor(pa, event.position);
                            this.view = Some(match zone {
                                AxisZone::Plot => view.zoom_at(factor, ax, 1.0 - ay),
                                AxisZone::XAxis => view.zoom_x(factor, ax),
                                AxisZone::YAxis => view.zoom_y(factor, 1.0 - ay),
                            });
                            cx.stop_propagation();
                            cx.notify();
                        },
                    ))
                    .child(
                        canvas(
                            {
                                let this = cx.entity().downgrade();
                                move |bounds, window, cx| {
                                    let pa = plot_area(bounds);
                                    let scale_factor = window.scale_factor();
                                    let (line_image, dropped) = this
                                        .update(cx, |this, _cx| {
                                            this.last_plot_area = Some(pa);
                                            let view = match view {
                                                Some(v) => v,
                                                None => {
                                                    return (
                                                        this.gpu_line_image.clone(),
                                                        std::mem::take(
                                                            &mut this.gpu_dropped_images,
                                                        ),
                                                    );
                                                }
                                            };
                                            if this.gpu_line.is_none() {
                                                this.gpu_line = gpu::LineRenderer::try_new();
                                            }
                                            if let Some(renderer) = this.gpu_line.as_mut() {
                                                let draws: Vec<gpu::LineDraw<'_>> = this
                                                    .traces
                                                    .iter()
                                                    .flatten()
                                                    .filter(|rt| rt.trace.visible)
                                                    .map(|rt| gpu::LineDraw {
                                                        component_id: rt.trace.component_id,
                                                        component: &rt.component,
                                                        element_index: rt.trace.element_index,
                                                        style: rt.trace.style,
                                                        color: rt.trace.color,
                                                        stroke_width: rt.trace.stroke_width,
                                                    })
                                                    .collect();
                                                if let Some(handle) = renderer.render_to_gpu(
                                                    pa,
                                                    view,
                                                    scale_factor,
                                                    &draws,
                                                ) {
                                                    _cx.spawn(async move |this, cx| {
                                                        let image = cx
                                                            .background_executor()
                                                            .spawn(
                                                                async move { handle.read_image() },
                                                            )
                                                            .await;
                                                        if let Some(img) = image {
                                                            let _ = this.update(cx, |this, cx| {
                                                                if let Some(prev) =
                                                                    this.gpu_line_image.replace(img)
                                                                {
                                                                    this.gpu_dropped_images
                                                                        .push(prev);
                                                                }
                                                                cx.notify();
                                                            });
                                                        }
                                                    })
                                                    .detach();
                                                }
                                            }
                                            (
                                                this.gpu_line_image.clone(),
                                                std::mem::take(&mut this.gpu_dropped_images),
                                            )
                                        })
                                        .unwrap_or((None, Vec::new()));
                                    for img in dropped {
                                        let _ = window.drop_image(img);
                                    }
                                    (bounds, view, line_image)
                                }
                            },
                            move |_, (bounds, view, line_image), window, cx| {
                                if let Some(view) = view {
                                    paint_plot(
                                        bounds,
                                        traces.iter().flatten(),
                                        view,
                                        data_start,
                                        line_image,
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
            let legend_bg = Hsla {
                a: 0.5,
                ..theme.bg_secondary
            };
            let mut legend_row = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .gap_y_0()
                .px(px(Y_LABEL_WIDTH + PADDING))
                //.py_1()
                .bg(legend_bg);

            for (i, rt) in self.traces.iter().enumerate() {
                let Some(rt) = rt else { continue };
                let visible = rt.trace.visible;
                let opacity = if visible { 1.0 } else { 0.3 };
                let color = Hsla {
                    a: opacity,
                    ..rt.trace.color
                };
                let text_color = Hsla {
                    a: opacity,
                    ..theme.text_secondary
                };

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
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _event: &gpui::MouseDownEvent, window, cx| {
                                if let Some(cb) = &this.on_open_page {
                                    let page = this.trace_inspect_page(i, cx);
                                    cb(page, window, cx);
                                }
                            }),
                        )
                        .child(div().w(px(10.0)).h(px(10.0)).rounded(px(2.0)).bg(color))
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
const TRACE_SUB_STROKE_WIDTH: u32 = 3;

impl Inspectable for TimeSeriesPlot {
    fn fields(&self) -> Vec<InspectionField> {
        let current_traces: Vec<(ComponentId, usize)> = self
            .resolved_traces()
            .map(|rt| (rt.trace.component_id, rt.trace.element_index))
            .collect();

        let mut fields = vec![
            InspectionField {
                label: "Title".into(),
                field_id: FieldId(5),
                value: InspectionValue::String(self.title().to_string()),
            },
            InspectionField {
                label: "Traces".into(),
                field_id: FieldId(0),
                value: InspectionValue::Traces(current_traces),
            },
        ];

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
                        InspectionField {
                            label: "Width".into(),
                            field_id: FieldId(base + TRACE_SUB_STROKE_WIDTH),
                            value: InspectionValue::F64(rt.trace.stroke_width as f64),
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
                let theme = crate::theme::theme(cx);
                let new_traces: Vec<Trace> = selections
                    .into_iter()
                    .enumerate()
                    .map(|(i, (component_id, element_index))| {
                        let elem_names = crate::trace_picker::element_names_for_component(
                            &self.db,
                            component_id,
                        );
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
                            stroke_width: 1.5,
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
            (FieldId(5), InspectionValue::String(s)) => {
                self.custom_title = if s.is_empty() { None } else { Some(s.into()) };
                cx.notify();
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
                        (TRACE_SUB_STROKE_WIDTH, InspectionValue::F64(w)) => {
                            resolved.trace.stroke_width = (w as f32).clamp(0.5, 10.0);
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
