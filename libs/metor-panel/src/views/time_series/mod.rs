use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    Bounds, Context, Entity, Hsla, IntoElement, MouseButton, PathBuilder, Pixels, Point,
    SharedString, Styled, TextRun, Window, canvas, div, point, prelude::*, px,
};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, PrimType};

#[allow(unused_imports)]
use crate::inspect;

mod bounds;
pub use bounds::*;

mod gpu;

mod line_plot;
pub use line_plot::LinePlot;

mod override_field;
pub use override_field::Override;

pub mod time_range;
pub use time_range::TimeRangeBehavior;

/// Iterator of Y-axis tick values within `view`.
///
/// Ticks are anchored at zero whenever the range crosses it so the origin
/// stays on a labeled line. Near-zero values are snapped to exact zero to
/// avoid the `-5.6e-17` artifacts that show up with floating arithmetic.
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
            let tick = if v.abs() < step * 1e-10 { 0.0 } else { v };
            v += step;
            Some(tick)
        } else {
            None
        }
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
/// human-readable offsets from `data_start`.
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

    // Ticks are snapped to multiples of `step_i` measured from `data_start`
    // so scrubbing doesn't make labels crawl across the axis.
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

/// Render a timestamp relative to `ref_us` as `"0"`, `"30 s"`, `"1 min 30 s"`, etc.
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
const X_LABEL_HEIGHT: f32 = 10.0;
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

/// Per-trace cache keyed by a node's `Arc::as_ptr` identity.
///
/// Stable across calls because sealed nodes are never replaced. Entries for
/// nodes that leave the component's live set are evicted on each call.
pub type NodeBoundsCache = HashMap<usize, NodeBounds>;

/// Aggregate `(min, max)` across `indexes` over every sample in
/// `component.time_series`, reusing `cache` so only new head-node samples
/// are scanned.
///
/// The inner loop dispatches on `PrimType` once per node and reads values
/// straight from the memory-mapped byte buffer, avoiding any per-sample
/// `ComponentView`, `ElementValue`, or `Result` allocation in the hot path.
pub fn expand_y_bounds(
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
    let mut live: HashSet<usize> = HashSet::new();

    for node in component.time_series.list.iter() {
        let node_id = Arc::as_ptr(&node) as usize;
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
fn paint_underlay(
    outer_bounds: Bounds<Pixels>,
    view: PlotBounds,
    data_start: f64,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let pb = plot_area(outer_bounds);
    let theme = crate::theme::theme(cx);

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

    if view.min_y < 0.0 && view.max_y > 0.0 {
        let zero_y = view.to_screen(pb, view.min_x, 0.0).y;
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
fn paint_overlay(
    outer_bounds: Bounds<Pixels>,
    view: PlotBounds,
    data_start: f64,
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

    // Semi-transparent axis fills mask any GPU-frame edge that strays into
    // the chrome strips.
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

    let t_min_i = data_start as i64;
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

/// Rendering mode for a single [`Trace`].
#[derive(Clone, Copy, Default, PartialEq, facet::Facet)]
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

/// Interactive wrapper around a [`LinePlot`] that adds axes, legend, and
/// pan/zoom input.
///
/// All plot state — traces, bounds tracking, view overrides, GPU resources
/// — lives in the inner [`LinePlot`] (also the Facet inspection target).
/// `TimeSeriesPlot` only owns drag state and chrome.
pub struct TimeSeriesPlot {
    line_plot: Entity<LinePlot>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotBounds>,
    drag_zone: AxisZone,
    last_plot_area: Option<Bounds<Pixels>>,
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
        Self {
            line_plot,
            drag_start: None,
            drag_start_view: None,
            drag_zone: AxisZone::Plot,
            last_plot_area: None,
        }
    }

    /// Convenience constructor: one trace per element, colors cycled from
    /// the theme's categorical palette, labels derived from element names.
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
        let elem_names = crate::inspector::trace_picker::element_names_for_component(&db, component_id);
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

    pub fn line_plot(&self) -> &Entity<LinePlot> {
        &self.line_plot
    }

    pub fn title(&self, cx: &gpui::App) -> SharedString {
        self.line_plot.read(cx).title()
    }

    /// Current effective view (auto-fit unless the user has overridden it).
    pub fn view(&self, cx: &gpui::App) -> Option<PlotBounds> {
        self.line_plot.read(cx).effective_view(cx)
    }

    /// Clear every user-imposed bound so auto-fit resumes on the next frame.
    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.line_plot.update(cx, |lp, cx| {
            lp.x_range = TimeRangeBehavior::default();
            lp.y_min_override = Override::Auto;
            lp.y_max_override = Override::Auto;
            lp.set_view_override(None, cx);
            cx.notify();
        });
    }
}

impl Render for TimeSeriesPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        let trace_entities: Vec<Entity<Trace>> = self.line_plot.read(cx).traces().to_vec();
        let show_legend = trace_entities.len() >= 2;

        let underlay_lp = self.line_plot.clone();
        let overlay_lp = self.line_plot.clone();

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_secondary)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                            if event.click_count == 2 {
                                this.reset_view(cx);
                            } else {
                                let zone = this
                                    .last_plot_area
                                    .map(|pa| axis_zone(event.position, pa))
                                    .unwrap_or(AxisZone::Plot);
                                this.drag_start = Some(event.position);
                                this.drag_start_view = this.line_plot.read(cx).effective_view(cx);
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
                            let new_view = match this.drag_zone {
                                AxisZone::Plot => start_view.offset_by_norm(-nx, ny),
                                AxisZone::XAxis => start_view.offset_x(-nx),
                                AxisZone::YAxis => start_view.offset_y(ny),
                            };
                            this.line_plot
                                .update(cx, |lp, cx| lp.set_view_override(Some(new_view), cx));
                        },
                    ))
                    .on_scroll_wheel(cx.listener(
                        |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                            let Some(view) = this.line_plot.read(cx).effective_view(cx) else {
                                return;
                            };
                            let Some(pa) = this.last_plot_area else {
                                return;
                            };

                            let delta = event.delta.pixel_delta(px(20.0));
                            let zoom_amount = f32::from(-delta.y) as f64 / 200.0;
                            let factor = (1.0_f64 + zoom_amount).clamp(0.5, 2.0);

                            let zone = axis_zone(event.position, pa);
                            let (ax, ay) = view.screen_anchor(pa, event.position);
                            let new_view = match zone {
                                AxisZone::Plot => view.zoom_at(factor, ax, 1.0 - ay),
                                AxisZone::XAxis => view.zoom_x(factor, ax),
                                AxisZone::YAxis => view.zoom_y(factor, 1.0 - ay),
                            };
                            this.line_plot
                                .update(cx, |lp, cx| lp.set_view_override(Some(new_view), cx));
                            cx.stop_propagation();
                        },
                    ))
                    .child(
                        canvas(
                            {
                                let this = cx.entity().downgrade();
                                move |bounds, _window, cx| {
                                    let _ = this.update(cx, |this, _| {
                                        this.last_plot_area = Some(plot_area(bounds));
                                    });
                                    let lp = underlay_lp.read(cx);
                                    (
                                        bounds,
                                        lp.effective_view(cx),
                                        lp.data_start().unwrap_or(0.0),
                                    )
                                }
                            },
                            move |_, (bounds, view, data_start), window, cx| {
                                if let Some(view) = view {
                                    paint_underlay(bounds, view, data_start, window, cx);
                                }
                            },
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(Y_LABEL_WIDTH + PADDING))
                            .top(px(PADDING))
                            .right(px(PADDING))
                            .bottom(px(X_LABEL_HEIGHT + PADDING))
                            .child(self.line_plot.clone()),
                    )
                    .child(
                        canvas(
                            move |bounds, _window, cx| {
                                let lp = overlay_lp.read(cx);
                                (
                                    bounds,
                                    lp.effective_view(cx),
                                    lp.data_start().unwrap_or(0.0),
                                )
                            },
                            move |_, (bounds, view, data_start), window, cx| {
                                if let Some(view) = view {
                                    paint_overlay(bounds, view, data_start, window, cx);
                                }
                            },
                        )
                        .absolute()
                        .inset_0(),
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
                .gap_1()
                .gap_y_0()
                .pl(px(Y_LABEL_WIDTH + PADDING))
                .pb_1()
                .bg(legend_bg);

            for trace_entity in trace_entities.iter() {
                let trace = trace_entity.read(cx);
                let visible = trace.visible;
                let opacity = if visible { 1.0 } else { 0.3 };
                let color = Hsla {
                    a: opacity,
                    ..trace.color
                };
                let text_color = Hsla {
                    a: opacity,
                    ..theme.text_secondary
                };
                let label = trace.label.clone();
                let toggle_target = trace_entity.clone();
                let inspect_target = trace_entity.clone();

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
                                toggle_target.update(cx, |t, cx| {
                                    t.visible = !t.visible;
                                    cx.notify();
                                });
                                this.line_plot.update(cx, |_, cx| cx.notify());
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |_this, event: &gpui::MouseDownEvent, window, cx| {
                                window.dispatch_action(
                                    Box::new(crate::inspector::InspectEntity {
                                        entity: inspect_target.clone().into_any(),
                                        position: event.position,
                                    }),
                                    cx,
                                );
                            }),
                        )
                        .child(div().w(px(10.0)).h(px(10.0)).rounded(px(2.0)).bg(color))
                        .child(
                            div()
                                .text_size(px(LABEL_FONT_SIZE))
                                .text_color(text_color)
                                .child(label),
                        ),
                );
            }

            root = root.child(legend_row);
        }

        root
    }
}
