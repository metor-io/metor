use std::sync::Arc;

use gpui::{
    canvas, div, point, prelude::*, px, Bounds, Context, Hsla, IntoElement, PathBuilder, Pixels,
    Point, SharedString, Styled, TextRun, Window,
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

/// Compute "nice" tick values that include 0 when the data range spans it.
/// Returns (axis_min, axis_max, ticks) where ticks are the values to label.
fn nice_ticks(data_min: f64, data_max: f64, target_count: usize) -> (f64, f64, Vec<f64>) {
    if data_min == data_max {
        let v = data_min;
        if v == 0.0 {
            return (-1.0, 1.0, vec![-1.0, 0.0, 1.0]);
        }
        let pad = v.abs() * 0.1;
        let lo = v - pad;
        let hi = v + pad;
        return (lo, hi, vec![lo, v, hi]);
    }

    let range = data_max - data_min;
    // Pick a "nice" step: 1, 2, or 5 times a power of 10
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

/// Compute nice time ticks (in microseconds). Returns tick positions.
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

struct PlotLayout {
    plot_bounds: Bounds<Pixels>,
    y_min: f64,
    y_max: f64,
    t_min: f64,
    t_max: f64,
}

impl PlotLayout {
    fn to_screen(&self, t: f64, v: f64) -> Point<Pixels> {
        let b = &self.plot_bounds;
        let nx = if self.t_max == self.t_min {
            0.5
        } else {
            (t - self.t_min) / (self.t_max - self.t_min)
        };
        let ny = if self.y_max == self.y_min {
            0.5
        } else {
            1.0 - (v - self.y_min) / (self.y_max - self.y_min)
        };
        point(
            b.origin.x + b.size.width * nx as f32,
            b.origin.y + b.size.height * ny as f32,
        )
    }
}

fn paint_plot(
    bounds: Bounds<Pixels>,
    points: &[(i64, f64)],
    color: Hsla,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    if points.len() < 2 {
        return;
    }

    let t_min_data = points[0].0;
    let t_max_data = points[points.len() - 1].0;
    if t_min_data == t_max_data {
        return;
    }

    let mut y_min_data = f64::INFINITY;
    let mut y_max_data = f64::NEG_INFINITY;
    for &(_, y) in points {
        if y < y_min_data {
            y_min_data = y;
        }
        if y > y_max_data {
            y_max_data = y;
        }
    }

    // Compute nice axis ranges
    let (y_axis_min, y_axis_max, y_ticks) = nice_ticks(y_min_data, y_max_data, 5);
    let x_ticks = nice_time_ticks(t_min_data, t_max_data, 5);

    // Reserve space for axis labels
    let label_font_size = px(11.0);
    let y_label_width = px(50.0);
    let x_label_height = px(20.0);
    let padding = px(8.0);

    let plot_bounds = Bounds {
        origin: point(
            bounds.origin.x + y_label_width + padding,
            bounds.origin.y + padding,
        ),
        size: gpui::Size {
            width: (bounds.size.width - y_label_width - padding * 2.0).max(px(1.0)),
            height: (bounds.size.height - x_label_height - padding * 2.0).max(px(1.0)),
        },
    };

    let layout = PlotLayout {
        plot_bounds,
        y_min: y_axis_min,
        y_max: y_axis_max,
        t_min: t_min_data as f64,
        t_max: t_max_data as f64,
    };

    let theme = &crate::theme::DARK;
    let axis_color = theme.axis_color;
    let grid_color = theme.grid_color;
    let label_color = theme.text_secondary;

    let text_style = window.text_style();
    let font = text_style.font();

    // Draw Y-axis grid lines, tick marks, and labels
    for &tick in &y_ticks {
        let screen = layout.to_screen(layout.t_min, tick);
        let y = screen.y;

        // Grid line
        let mut grid = PathBuilder::stroke(px(0.5));
        grid.move_to(point(plot_bounds.origin.x, y));
        grid.line_to(point(
            plot_bounds.origin.x + plot_bounds.size.width,
            y,
        ));
        if let Ok(path) = grid.build() {
            window.paint_path(path, grid_color);
        }

        // Label
        let label_text = format_value_label(tick);
        let label_shared = SharedString::from(label_text.clone());
        let run = TextRun {
            len: label_text.len(),
            font: font.clone(),
            color: label_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window
            .text_system()
            .shape_line(label_shared, label_font_size, &[run], None);
        let label_width = shaped.width;
        let label_origin = point(
            plot_bounds.origin.x - label_width - px(4.0),
            y - label_font_size / 2.0,
        );
        let _ = shaped.paint(label_origin, label_font_size, window, cx);
    }

    // Draw X-axis tick marks and labels
    for &tick in &x_ticks {
        let screen = layout.to_screen(tick as f64, layout.y_min);
        let x = screen.x;

        // Grid line
        let mut grid = PathBuilder::stroke(px(0.5));
        grid.move_to(point(x, plot_bounds.origin.y));
        grid.line_to(point(
            x,
            plot_bounds.origin.y + plot_bounds.size.height,
        ));
        if let Ok(path) = grid.build() {
            window.paint_path(path, grid_color);
        }

        // Label
        let label_text = format_time_label(tick, t_min_data);
        let label_shared = SharedString::from(label_text.clone());
        let run = TextRun {
            len: label_text.len(),
            font: font.clone(),
            color: label_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window
            .text_system()
            .shape_line(label_shared, label_font_size, &[run], None);
        let label_origin = point(
            x - shaped.width / 2.0,
            plot_bounds.origin.y + plot_bounds.size.height + px(4.0),
        );
        let _ = shaped.paint(label_origin, label_font_size, window, cx);
    }

    // Draw zero line if visible
    if y_axis_min < 0.0 && y_axis_max > 0.0 {
        let zero_y = layout.to_screen(layout.t_min, 0.0).y;
        let mut zero_path = PathBuilder::stroke(px(1.0));
        zero_path.move_to(point(plot_bounds.origin.x, zero_y));
        zero_path.line_to(point(
            plot_bounds.origin.x + plot_bounds.size.width,
            zero_y,
        ));
        if let Ok(path) = zero_path.build() {
            window.paint_path(path, theme.zero_line_color);
        }
    }

    // Draw axis lines
    let mut axes = PathBuilder::stroke(px(1.0));
    // Y axis
    axes.move_to(point(plot_bounds.origin.x, plot_bounds.origin.y));
    axes.line_to(point(
        plot_bounds.origin.x,
        plot_bounds.origin.y + plot_bounds.size.height,
    ));
    // X axis
    axes.line_to(point(
        plot_bounds.origin.x + plot_bounds.size.width,
        plot_bounds.origin.y + plot_bounds.size.height,
    ));
    if let Ok(path) = axes.build() {
        window.paint_path(path, axis_color);
    }

    // Draw data line
    let mut path = PathBuilder::stroke(px(1.5));
    let first = layout.to_screen(points[0].0 as f64, points[0].1);
    path.move_to(first);
    for &(t, v) in &points[1..] {
        path.line_to(layout.to_screen(t as f64, v));
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

pub struct TimeSeriesPlot {
    points: Vec<(i64, f64)>,
    color: Hsla,
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
            _task: task,
        }
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = color;
        self
    }
}

impl Render for TimeSeriesPlot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let points = self.points.clone();
        let color = self.color;
        div()
            .size_full()
            .bg(crate::theme::DARK.bg_secondary)
            .child(
                canvas(
                    move |bounds, _, _| (bounds, points),
                    move |_, (bounds, points), window, cx| {
                        paint_plot(bounds, &points, color, window, cx);
                    },
                )
                .size_full(),
            )
    }
}
