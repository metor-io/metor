use std::sync::Arc;

use gpui::{
    canvas, div, point, prelude::*, px, Bounds, Context, Hsla, IntoElement, MouseButton,
    PathBuilder, Pixels, Point, SharedString, Styled, TextRun, Window,
};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, ComponentView};

use crate::wait_for_component;

fn component_view_to_f64(view: ComponentView<'_>) -> f64 {
    match view {
        ComponentView::U8(v) => v.buf()[0] as f64,
        ComponentView::U16(v) => v.buf()[0] as f64,
        ComponentView::U32(v) => v.buf()[0] as f64,
        ComponentView::U64(v) => v.buf()[0] as f64,
        ComponentView::I8(v) => v.buf()[0] as f64,
        ComponentView::I16(v) => v.buf()[0] as f64,
        ComponentView::I32(v) => v.buf()[0] as f64,
        ComponentView::I64(v) => v.buf()[0] as f64,
        ComponentView::F32(v) => v.buf()[0] as f64,
        ComponentView::F64(v) => v.buf()[0],
        ComponentView::Bool(v) => v.buf()[0] as u8 as f64,
    }
}

fn read_time_series(component: &Component) -> Vec<(i64, f64)> {
    let mut points = Vec::new();
    let ts = &component.time_series;
    let element_size = component.schema.size();
    for node in ts.list.iter() {
        let timestamps = node.timestamps();
        let data = node.data.data();
        for (i, timestamp) in timestamps.iter().enumerate() {
            let start = i * element_size;
            let end = start + element_size;
            let Some(buf) = data.get(start..end) else {
                break;
            };
            if let Ok((_size, view)) = component.schema.parse_value(buf) {
                points.push((timestamp.0, component_view_to_f64(view)));
            }
        }
    }
    points.sort_by_key(|p| p.0);
    points
}

// --- PlotBounds ---

#[derive(Clone, Copy, Debug)]
pub struct PlotBounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl PlotBounds {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    pub fn from_points(points: &[(i64, f64)]) -> Option<Self> {
        if points.len() < 2 {
            return None;
        }
        let min_x = points[0].0 as f64;
        let max_x = points[points.len() - 1].0 as f64;
        if min_x == max_x {
            return None;
        }
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for &(_, y) in points {
            if y < min_y {
                min_y = y;
            }
            if y > max_y {
                max_y = y;
            }
        }
        Some(Self::new(min_x, min_y, max_x, max_y).normalize())
    }

    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    pub fn offset(mut self, dx: f64, dy: f64) -> Self {
        self.min_x += dx;
        self.max_x += dx;
        self.min_y += dy;
        self.max_y += dy;
        self
    }

    /// Pan by a normalized amount (0..1 = full width/height).
    pub fn offset_by_norm(self, nx: f64, ny: f64) -> Self {
        self.offset(nx * self.width(), ny * self.height())
    }

    /// Zoom around an anchor point (0..1 normalized within the bounds).
    pub fn zoom_at(self, factor: f64, anchor_x: f64, anchor_y: f64) -> Self {
        let dw = self.width() * (factor - 1.0);
        let dh = self.height() * (factor - 1.0);
        Self {
            min_x: self.min_x - dw * anchor_x,
            max_x: self.max_x + dw * (1.0 - anchor_x),
            min_y: self.min_y - dh * anchor_y,
            max_y: self.max_y + dh * (1.0 - anchor_y),
        }
    }

    /// Ensure min < max with at least a span of 1.0.
    pub fn normalize(mut self) -> Self {
        if self.min_x >= self.max_x {
            self.min_x = self.max_x.min(self.min_x);
            self.max_x = self.min_x + 1.0;
        }
        if self.min_y >= self.max_y {
            self.min_y = self.max_y.min(self.min_y);
            self.max_y = self.min_y + 1.0;
        }
        self
    }

    /// Convert a screen pixel position to data coordinates given the pixel bounds of the plot.
    pub fn screen_to_data(&self, screen_bounds: Bounds<Pixels>, pos: Point<Pixels>) -> (f64, f64) {
        let nx = (f32::from(pos.x - screen_bounds.origin.x)
            / f32::from(screen_bounds.size.width)) as f64;
        let ny = (f32::from(pos.y - screen_bounds.origin.y)
            / f32::from(screen_bounds.size.height)) as f64;
        (self.min_x + nx * self.width(), self.max_y - ny * self.height())
    }

    /// Convert a screen pixel delta to a normalized (0..1) fraction of the plot.
    pub fn screen_delta_to_norm(
        &self,
        screen_bounds: Bounds<Pixels>,
        dx: Pixels,
        dy: Pixels,
    ) -> (f64, f64) {
        let nx = f32::from(dx) as f64 / f32::from(screen_bounds.size.width) as f64;
        let ny = f32::from(dy) as f64 / f32::from(screen_bounds.size.height) as f64;
        (nx, ny)
    }

    /// Normalized position of a screen point within the plot (0..1).
    pub fn screen_anchor(
        &self,
        screen_bounds: Bounds<Pixels>,
        pos: Point<Pixels>,
    ) -> (f64, f64) {
        let nx = (f32::from(pos.x - screen_bounds.origin.x)
            / f32::from(screen_bounds.size.width)) as f64;
        let ny = (f32::from(pos.y - screen_bounds.origin.y)
            / f32::from(screen_bounds.size.height)) as f64;
        (nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0))
    }

    fn to_screen(&self, screen_bounds: Bounds<Pixels>, data_x: f64, data_y: f64) -> Point<Pixels> {
        let nx = if self.width() == 0.0 {
            0.5
        } else {
            (data_x - self.min_x) / self.width()
        };
        let ny = if self.height() == 0.0 {
            0.5
        } else {
            1.0 - (data_y - self.min_y) / self.height()
        };
        point(
            screen_bounds.origin.x + screen_bounds.size.width * nx as f32,
            screen_bounds.origin.y + screen_bounds.size.height * ny as f32,
        )
    }
}

// --- Tick computation ---

fn nice_ticks(data_min: f64, data_max: f64, target_count: usize) -> (f64, f64, Vec<f64>) {
    if data_min == data_max {
        let v = data_min;
        if v == 0.0 {
            return (-1.0, 1.0, vec![-1.0, 0.0, 1.0]);
        }
        let pad = v.abs() * 0.1;
        return (v - pad, v + pad, vec![v - pad, v, v + pad]);
    }

    let range = data_max - data_min;
    let rough_step = range / target_count as f64;
    let mag = 10f64.powf(rough_step.log10().floor());
    let norm = rough_step / mag;
    let nice_step = if norm <= 1.5 {
        mag
    } else if norm <= 3.0 {
        2.0 * mag
    } else if norm <= 7.0 {
        5.0 * mag
    } else {
        10.0 * mag
    };

    let axis_min = (data_min / nice_step).floor() * nice_step;
    let axis_max = (data_max / nice_step).ceil() * nice_step;

    let mut ticks = Vec::new();
    let mut v = axis_min;
    while v <= axis_max + nice_step * 0.5 {
        ticks.push(v);
        v += nice_step;
    }
    (axis_min, axis_max, ticks)
}

fn nice_time_ticks(t_min: i64, t_max: i64, target_count: usize) -> Vec<i64> {
    let range = (t_max - t_min) as f64;
    if range <= 0.0 {
        return vec![t_min];
    }
    let rough_step = range / target_count as f64;
    let mag = 10f64.powf(rough_step.log10().floor());
    let norm = rough_step / mag;
    let nice_step = if norm <= 1.5 {
        mag
    } else if norm <= 3.0 {
        2.0 * mag
    } else if norm <= 7.0 {
        5.0 * mag
    } else {
        10.0 * mag
    };
    let step = (nice_step as i64).max(1);

    let start = (t_min / step) * step;
    let mut ticks = Vec::new();
    let mut t = start;
    while t <= t_max {
        if t >= t_min {
            ticks.push(t);
        }
        t += step;
    }
    if ticks.is_empty() {
        ticks.push(t_min);
    }
    ticks
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

// --- Plot area layout ---

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

// --- Painting ---

fn paint_plot(
    outer_bounds: Bounds<Pixels>,
    points: &[(i64, f64)],
    view: PlotBounds,
    color: Hsla,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    if points.is_empty() {
        return;
    }

    let (y_axis_min, y_axis_max, y_ticks) = nice_ticks(view.min_y, view.max_y, 5);
    let x_ticks = nice_time_ticks(view.min_x as i64, view.max_x as i64, 5);

    let pb = plot_area(outer_bounds);
    let axis_view = PlotBounds::new(view.min_x, y_axis_min, view.max_x, y_axis_max);

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

    // Clipped region: grid, zero line, data
    window.with_content_mask(Some(gpui::ContentMask { bounds: pb }), |window| {
        // Y grid
        for &tick in &y_ticks {
            let y = axis_view.to_screen(pb, axis_view.min_x, tick).y;
            let mut grid = PathBuilder::stroke(px(0.5));
            grid.move_to(point(pb.origin.x, y));
            grid.line_to(point(pb.origin.x + pb.size.width, y));
            if let Ok(path) = grid.build() {
                window.paint_path(path, theme.grid_color);
            }
        }

        // X grid
        for &tick in &x_ticks {
            let x = axis_view.to_screen(pb, tick as f64, axis_view.min_y).x;
            let mut grid = PathBuilder::stroke(px(0.5));
            grid.move_to(point(x, pb.origin.y));
            grid.line_to(point(x, pb.origin.y + pb.size.height));
            if let Ok(path) = grid.build() {
                window.paint_path(path, theme.grid_color);
            }
        }

        // Zero line
        if axis_view.min_y < 0.0 && axis_view.max_y > 0.0 {
            let zero_y = axis_view.to_screen(pb, axis_view.min_x, 0.0).y;
            let mut zp = PathBuilder::stroke(px(1.0));
            zp.move_to(point(pb.origin.x, zero_y));
            zp.line_to(point(pb.origin.x + pb.size.width, zero_y));
            if let Ok(path) = zp.build() {
                window.paint_path(path, theme.zero_line_color);
            }
        }

        // Data line
        if points.len() >= 2 {
            let mut path = PathBuilder::stroke(px(1.5));
            let first = view.to_screen(pb, points[0].0 as f64, points[0].1);
            path.move_to(first);
            for &(t, v) in &points[1..] {
                path.line_to(view.to_screen(pb, t as f64, v));
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        }
    });

    // Axis frame (unclipped)
    let mut axes = PathBuilder::stroke(px(1.0));
    axes.move_to(point(pb.origin.x, pb.origin.y));
    axes.line_to(point(pb.origin.x, pb.origin.y + pb.size.height));
    axes.line_to(point(pb.origin.x + pb.size.width, pb.origin.y + pb.size.height));
    if let Ok(path) = axes.build() {
        window.paint_path(path, theme.axis_color);
    }

    // Y labels
    for &tick in &y_ticks {
        let y = axis_view.to_screen(pb, axis_view.min_x, tick).y;
        if y < pb.origin.y || y > pb.origin.y + pb.size.height {
            continue;
        }
        let text = format_value_label(tick);
        let run = make_run(&text);
        let shaped = window
            .text_system()
            .shape_line(SharedString::from(text), label_font_size, &[run], None);
        let origin = point(pb.origin.x - shaped.width - px(4.0), y - label_font_size / 2.0);
        let _ = shaped.paint(origin, label_font_size, window, cx);
    }

    // X labels
    for &tick in &x_ticks {
        let x = axis_view.to_screen(pb, tick as f64, axis_view.min_y).x;
        if x < pb.origin.x || x > pb.origin.x + pb.size.width {
            continue;
        }
        let text = format_time_label(tick, view.min_x as i64);
        let run = make_run(&text);
        let shaped = window
            .text_system()
            .shape_line(SharedString::from(text), label_font_size, &[run], None);
        let origin = point(x - shaped.width / 2.0, pb.origin.y + pb.size.height + px(4.0));
        let _ = shaped.paint(origin, label_font_size, window, cx);
    }
}

// --- TimeSeriesPlot view ---

pub struct TimeSeriesPlot {
    points: Vec<(i64, f64)>,
    color: Hsla,
    /// Current viewport. None means auto-fit to data.
    view: Option<PlotBounds>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotBounds>,
    last_plot_area: Option<Bounds<Pixels>>,
    _task: gpui::Task<()>,
}

impl TimeSeriesPlot {
    pub fn new(
        db: Arc<DB>,
        component_id: impl Into<ComponentId> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let component_id = component_id.into();
        let task = cx.spawn(async move |this, cx| {
            let component = wait_for_component(&db, component_id).await;
            loop {
                let points = read_time_series(&component);
                let result = this.update(cx, |this, cx| {
                    this.points = points;
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                component.time_series.wait().await;
            }
        });
        Self {
            points: Vec::new(),
            color: crate::theme::DARK.line_color,
            view: None,
            drag_start: None,
            drag_start_view: None,
            last_plot_area: None,
            _task: task,
        }
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = color;
        self
    }

    fn current_view(&self) -> Option<PlotBounds> {
        self.view.or_else(|| PlotBounds::from_points(&self.points))
    }
}

impl Render for TimeSeriesPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let points = self.points.clone();
        let color = self.color;
        let view = self.current_view();

        div()
            .size_full()
            .bg(crate::theme::DARK.bg_secondary)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, _cx| {
                    this.drag_start = Some(event.position);
                    this.drag_start_view = this.current_view();
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
                    // Invert: dragging right pans data left, dragging down pans data up
                    this.view = Some(start_view.offset_by_norm(-nx, ny));
                    cx.notify();
                },
            ))
            .on_scroll_wheel(cx.listener(
                |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let (Some(view), Some(pa)) = (this.current_view(), this.last_plot_area) else {
                        return;
                    };

                    let delta = event.delta.pixel_delta(px(20.0));
                    let zoom_amount = f32::from(-delta.y) as f64 / 200.0;
                    let factor = (1.0_f64 + zoom_amount).clamp(0.5, 2.0);

                    let (ax, ay) = view.screen_anchor(pa, event.position);
                    // Invert Y anchor: screen Y is top-down, data Y is bottom-up
                    this.view = Some(view.zoom_at(factor, ax, 1.0 - ay));
                    cx.notify();
                },
            ))
            .child(
                canvas(
                    {
                        let this = cx.entity().downgrade();
                        move |bounds, _window, cx| {
                            let pa = plot_area(bounds);
                            let _ = this.update(cx, |this, _cx| {
                                this.last_plot_area = Some(pa);
                            });
                            (bounds, points, view)
                        }
                    },
                    move |_, (bounds, points, view), window, cx| {
                        if let Some(view) = view {
                            paint_plot(bounds, &points, view, color, window, cx);
                        }
                    },
                )
                .size_full(),
            )
    }
}
