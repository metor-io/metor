use std::sync::Arc;

use gpui::{
    Bounds, Context, Hsla, IntoElement, MouseButton, PathBuilder, Pixels, Point, SharedString,
    Styled, TextRun, Window, canvas, div, point, prelude::*, px,
};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, Timestamp};

use crate::inspectable::{FieldId, Inspectable, InspectionField, InspectionValue};
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

/// Generate X-axis tick positions (timestamps in microseconds) within the visible bounds.
fn x_ticks(view: &PlotBounds, target_count: usize) -> impl Iterator<Item = i64> {
    let range = view.width();
    let step = pretty_round(range / target_count as f64);
    let valid = range > 0.0 && step.is_normal() && step > 0.0;

    let step_i = if valid { (step as i64).max(1) } else { 1 };
    let t_min = view.min_x as i64;
    let t_max = view.max_x as i64;

    // Align start to multiples of step, then skip below t_min
    let aligned = if valid {
        (t_min / step_i) * step_i
    } else {
        t_min
    };
    let start = if aligned < t_min {
        aligned + step_i
    } else {
        aligned
    };
    // If !valid, produce exactly one tick at t_min
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

fn format_time_label(t_us: i64, t_min: i64) -> String {
    let relative_us = t_us - t_min;
    let secs = relative_us as f64 / 1_000_000.0;
    if secs.abs() < 100.0 {
        format!("{:.1}s", secs)
    } else {
        format!("{:.0}s", secs)
    }
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
    let t_min_i = view.min_x as i64;
    for tick in x_ticks(&view, 5) {
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

    // Axis frame
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

    // Data lines — clipped to plot area
    window.with_content_mask(Some(gpui::ContentMask { bounds: pb }), |window| {
        for rt in traces {
            paint_data_line(
                pb,
                &rt.component,
                &view,
                rt.trace.color,
                px(1.5),
                rt.trace.element_index,
                window,
            );
        }
    });
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
    if let Some(slice) = component.time_series.get_range(start_ts..end_ts) {
        let schema = &component.schema;
        let node_slices: Vec<_> = slice.as_iter().collect();

        let mut path = PathBuilder::stroke(stroke_width);
        let mut first = true;

        for node_slice in node_slices.iter().rev() {
            for (ts, cv) in node_slice.iter_values(schema) {
                let v = match cv.get(element_index) {
                    Some(ev) => ev.as_f64(),
                    None => continue,
                };
                let screen_pt = view.to_screen(screen_bounds, ts.0 as f64, v);
                if first {
                    path.move_to(screen_pt);
                    first = false;
                } else {
                    path.line_to(screen_pt);
                }
            }
        }

        if !first {
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        }
    }
}

/// A single line on a time series plot: one element index from one component.
#[derive(Clone)]
pub struct Trace {
    pub component_id: ComponentId,
    pub element_index: usize,
    pub color: Hsla,
}

impl Trace {
    pub fn new(component_id: impl Into<ComponentId>, element_index: usize, color: Hsla) -> Self {
        Self {
            component_id: component_id.into(),
            element_index,
            color,
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

pub struct TimeSeriesPlot {
    db: Arc<DB>,
    traces: Vec<Option<ResolvedTrace>>,
    view: Option<PlotBounds>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotBounds>,
    last_plot_area: Option<Bounds<Pixels>>,
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
            _tasks: tasks,
        }
    }

    /// Convenience: create a plot for a single component with the given element
    /// indexes, auto-assigning colors from the theme palette.
    pub fn from_component(
        db: Arc<DB>,
        component_id: impl Into<ComponentId>,
        indexes: Vec<usize>,
        cx: &mut Context<Self>,
    ) -> Self {
        let component_id = component_id.into();
        let indexes = if indexes.is_empty() { vec![0] } else { indexes };
        let theme = &crate::theme::DARK;
        let traces = indexes
            .iter()
            .enumerate()
            .map(|(i, &idx)| Trace {
                component_id,
                element_index: idx,
                color: theme.line_colors[i % theme.line_colors.len()],
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

    fn current_view(&self) -> Option<PlotBounds> {
        self.view.or_else(|| {
            let (start, end) = self.time_range()?;
            let (min_y, max_y) = self.merged_y_bounds()?;
            Some(PlotBounds::new(start, min_y, end, max_y).normalize())
        })
    }

    fn trace_label(&self, trace: &Trace) -> String {
        let name = self
            .db
            .with_state(|state| {
                state
                    .get_component_metadata(trace.component_id)
                    .map(|m| m.name.clone())
            })
            .unwrap_or_else(|| trace.component_id.to_string());
        format!("{}[{}]", name, trace.element_index)
    }
}

impl Render for TimeSeriesPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.current_view();
        let traces = self.traces.clone();

        div()
            .size_full()
            .bg(crate::theme::DARK.bg_secondary)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                    if event.click_count == 2 {
                        this.view = None;
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
                    let (Some(view), Some(pa)) = (this.current_view(), this.last_plot_area) else {
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
                            paint_plot(bounds, traces.iter().flatten(), view, window, cx);
                        }
                    },
                )
                .size_full(),
            )
    }
}

impl Inspectable for TimeSeriesPlot {
    fn fields(&self) -> Vec<InspectionField> {
        let traces_str = self
            .resolved_traces()
            .map(|rt| self.trace_label(&rt.trace))
            .collect::<Vec<_>>()
            .join(", ");

        let mut fields = vec![InspectionField {
            label: "Traces".into(),
            field_id: FieldId(0),
            value: InspectionValue::String(traces_str),
        }];

        if let Some((min, max)) = self.merged_y_bounds() {
            fields.push(InspectionField {
                label: "Y Min".into(),
                field_id: FieldId(1),
                value: InspectionValue::F64(min),
            });
            fields.push(InspectionField {
                label: "Y Max".into(),
                field_id: FieldId(2),
                value: InspectionValue::F64(max),
            });
        }
        fields
    }

    fn set_field(&mut self, field_id: FieldId, value: InspectionValue, cx: &mut Context<Self>) {
        match (field_id, value) {
            (FieldId(0), InspectionValue::String(s)) => {
                // Parse "name[idx], name[idx], ..."
                let theme = &crate::theme::DARK;
                let new_traces: Vec<Trace> = s
                    .split(',')
                    .filter_map(|part| {
                        let part = part.trim();
                        let (name, rest) = part.split_once('[')?;
                        let idx_str = rest.strip_suffix(']')?;
                        let idx: usize = idx_str.parse().ok()?;
                        let component_id = self.db.with_state(|state| {
                            state
                                .component_metadata_iter()
                                .find(|(_, m)| m.name == name.trim())
                                .map(|(id, _)| *id)
                        })?;
                        Some((component_id, idx))
                    })
                    .enumerate()
                    .map(|(i, (component_id, idx))| Trace {
                        component_id,
                        element_index: idx,
                        color: theme.line_colors[i % theme.line_colors.len()],
                    })
                    .collect();

                if !new_traces.is_empty() {
                    self.set_traces(new_traces, cx);
                }
            }
            (FieldId(1), InspectionValue::F64(_v)) => {
                // Y Min override — not implemented for multi-trace yet
            }
            (FieldId(2), InspectionValue::F64(_v)) => {
                // Y Max override — not implemented for multi-trace yet
            }
            _ => {}
        }
    }
}
