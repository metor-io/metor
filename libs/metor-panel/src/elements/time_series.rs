use std::sync::Arc;

use gpui::{
    Bounds, Context, Hsla, IntoElement, MouseButton, PathBuilder, Pixels, Point, SharedString,
    Styled, TextRun, Window, canvas, div, point, prelude::*, px,
};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, Timestamp};

use crate::inspectable::{FieldId, Inspectable, InspectionField, InspectionValue};
use crate::wait_for_component;

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

    pub fn offset_by_norm(self, nx: f64, ny: f64) -> Self {
        self.offset(nx * self.width(), ny * self.height())
    }

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

    pub fn screen_anchor(&self, screen_bounds: Bounds<Pixels>, pos: Point<Pixels>) -> (f64, f64) {
        let nx = (f32::from(pos.x - screen_bounds.origin.x) / f32::from(screen_bounds.size.width))
            as f64;
        let ny = (f32::from(pos.y - screen_bounds.origin.y) / f32::from(screen_bounds.size.height))
            as f64;
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

/// Round a step size to a "pretty" value (nearest 0.5 at the appropriate magnitude).
fn pretty_round(num: f64) -> f64 {
    if num == 0.0 || !num.is_finite() {
        return num;
    }
    let mut multiplier = 1.0;
    let mut n = num.abs();

    while n < 1.0 {
        n *= 10.0;
        multiplier *= 10.0;
    }

    let rounded = (n * 2.0).round() / 2.0;
    let result = rounded / multiplier;
    if num < 0.0 { -result } else { result }
}

/// Generate Y-axis tick positions within the visible bounds.
/// When the range spans 0, ticks are anchored at 0 and extend outward.
fn y_ticks(view: &PlotBounds, target_count: usize) -> Vec<f64> {
    let step = pretty_round(view.height() / target_count as f64);
    if !step.is_normal() || step <= 0.0 {
        return vec![];
    }

    let mut ticks = Vec::new();

    if view.min_y <= 0.0 && view.max_y >= 0.0 {
        // Walk outward from 0 in both directions
        let mut v = 0.0;
        while v <= view.max_y {
            ticks.push(v);
            v += step;
        }
        let mut v = -step;
        while v >= view.min_y {
            ticks.push(v);
            v -= step;
        }
    } else {
        // Walk from a step-aligned start
        let start = (view.min_y / step).floor() * step;
        let mut v = start;
        while v <= view.max_y + step * 0.01 {
            if v >= view.min_y {
                ticks.push(v);
            }
            v += step;
        }
    }

    ticks.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ticks
}

/// Generate X-axis tick positions (timestamps in microseconds) within the visible bounds.
/// Ticks are anchored at 0 relative to view.min_x and stepped by a pretty-rounded interval.
fn x_ticks(view: &PlotBounds, target_count: usize) -> Vec<i64> {
    let range = view.width();
    if range <= 0.0 {
        return vec![view.min_x as i64];
    }

    let step = pretty_round(range / target_count as f64);
    if !step.is_normal() || step <= 0.0 {
        return vec![view.min_x as i64];
    }
    let step_i = (step as i64).max(1);
    let t_min = view.min_x as i64;
    let t_max = view.max_x as i64;

    // Align to multiples of step
    let start = (t_min / step_i) * step_i;
    let mut ticks = Vec::new();
    let mut t = start;
    while t <= t_max {
        if t >= t_min {
            ticks.push(t);
        }
        t += step_i;
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

// --- Y-bounds computation ---

fn compute_y_bounds(component: &Component) -> Option<(f64, f64)> {
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let element_size = component.schema.size();
    let mut any = false;
    for node in component.time_series.list.iter() {
        let data = node.data.data();
        let count = node.timestamps().len();
        for i in 0..count {
            let start = i * element_size;
            if let Some(buf) = data.get(start..start + element_size) {
                if let Ok((_size, view)) = component.schema.parse_value(buf) {
                    let v = view.to_f64();
                    min_y = min_y.min(v);
                    max_y = max_y.max(v);
                    any = true;
                }
            }
        }
    }
    any.then_some((min_y, max_y))
}

// --- Painting ---

fn paint_plot(
    outer_bounds: Bounds<Pixels>,
    component: &Component,
    view: PlotBounds,
    color: Hsla,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let y_tick_values = y_ticks(&view, 5);
    let x_tick_values = x_ticks(&view, 5);

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

    // Clipped region: grid, zero line, data
    window.with_content_mask(Some(gpui::ContentMask { bounds: pb }), |window| {
        // Y grid
        for &tick in &y_tick_values {
            let y = view.to_screen(pb, view.min_x, tick).y;
            let mut grid = PathBuilder::stroke(px(0.5));
            grid.move_to(point(pb.origin.x, y));
            grid.line_to(point(pb.origin.x + pb.size.width, y));
            if let Ok(path) = grid.build() {
                window.paint_path(path, theme.grid_color);
            }
        }

        // X grid
        for &tick in &x_tick_values {
            let x = view.to_screen(pb, tick as f64, view.min_y).x;
            let mut grid = PathBuilder::stroke(px(0.5));
            grid.move_to(point(x, pb.origin.y));
            grid.line_to(point(x, pb.origin.y + pb.size.height));
            if let Ok(path) = grid.build() {
                window.paint_path(path, theme.grid_color);
            }
        }

        // Zero line
        if view.min_y < 0.0 && view.max_y > 0.0 {
            let zero_y = view.to_screen(pb, view.min_x, 0.0).y;
            let mut zp = PathBuilder::stroke(px(1.0));
            zp.move_to(point(pb.origin.x, zero_y));
            zp.line_to(point(pb.origin.x + pb.size.width, zero_y));
            if let Ok(path) = zp.build() {
                window.paint_path(path, theme.zero_line_color);
            }
        }

        // Data line — iterate directly from TimeSeries
        let start_ts = Timestamp(view.min_x as i64);
        let end_ts = Timestamp(view.max_x as i64);
        if let Some(slice) = component.time_series.get_range(start_ts..end_ts) {
            let schema = &component.schema;
            let node_slices: Vec<_> = slice.as_iter().collect();

            let mut path = PathBuilder::stroke(px(1.5));
            let mut first = true;

            for node_slice in node_slices.iter().rev() {
                for (ts, cv) in node_slice.iter_values(schema) {
                    let screen_pt = view.to_screen(pb, ts.0 as f64, cv.to_f64());
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
    });

    // Axis frame (unclipped)
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

    // Y labels (unclipped)
    for &tick in &y_tick_values {
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

    // X labels (unclipped)
    for &tick in &x_tick_values {
        let x = view.to_screen(pb, tick as f64, view.min_y).x;
        if x < pb.origin.x || x > pb.origin.x + pb.size.width {
            continue;
        }
        let text = format_time_label(tick, view.min_x as i64);
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
}

// --- TimeSeriesPlot view ---

pub struct TimeSeriesPlot {
    db: Arc<DB>,
    component: Option<Component>,
    component_id: ComponentId,
    y_bounds: Option<(f64, f64)>,
    color: Hsla,
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
        let task = Self::spawn_stream(db.clone(), component_id, cx);
        Self {
            db,
            component: None,
            component_id,
            y_bounds: None,
            color: crate::theme::DARK.line_color,
            view: None,
            drag_start: None,
            drag_start_view: None,
            last_plot_area: None,
            _task: task,
        }
    }

    fn spawn_stream(
        db: Arc<DB>,
        component_id: ComponentId,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let component = wait_for_component(&db, component_id).await;

            let result = this.update(cx, |this, cx| {
                this.component = Some(component.clone());
                cx.notify();
            });
            if result.is_err() {
                return;
            }

            loop {
                let y_bounds = compute_y_bounds(&component);
                let result = this.update(cx, |this, cx| {
                    this.y_bounds = y_bounds;
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                component.time_series.wait().await;
            }
        })
    }

    pub fn set_component(&mut self, component_id: ComponentId, cx: &mut Context<Self>) {
        self.component_id = component_id;
        self.component = None;
        self.y_bounds = None;
        self.view = None;
        self._task = Self::spawn_stream(self.db.clone(), component_id, cx);
        cx.notify();
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = color;
        self
    }

    fn current_view(&self) -> Option<PlotBounds> {
        self.view.or_else(|| {
            let component = self.component.as_ref()?;
            let ts = &component.time_series;
            let start = ts.start_timestamp()?.0 as f64;
            let end = ts.latest()?.timestamp().0 as f64;
            if start == end {
                return None;
            }
            let (min_y, max_y) = self.y_bounds?;
            Some(PlotBounds::new(start, min_y, end, max_y).normalize())
        })
    }
}

impl Render for TimeSeriesPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let component = self.component.clone();
        let color = self.color;
        let view = self.current_view();

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
                            (bounds, component, view)
                        }
                    },
                    move |_, (bounds, component, view), window, cx| {
                        if let (Some(component), Some(view)) = (component, view) {
                            paint_plot(bounds, &component, view, color, window, cx);
                        }
                    },
                )
                .size_full(),
            )
    }
}

impl Inspectable for TimeSeriesPlot {
    fn fields(&self) -> Vec<InspectionField> {
        let component_name = self
            .db
            .with_state(|state| {
                state
                    .get_component_metadata(self.component_id)
                    .map(|m| m.name.clone())
            })
            .unwrap_or_else(|| self.component_id.to_string());

        let mut fields = vec![
            InspectionField {
                label: "Component".into(),
                field_id: FieldId(0),
                value: InspectionValue::Component {
                    name: component_name,
                },
            },
            InspectionField {
                label: "Color".into(),
                field_id: FieldId(1),
                value: InspectionValue::Color(self.color),
            },
        ];
        if let Some((min, max)) = self.y_bounds {
            fields.push(InspectionField {
                label: "Y Min".into(),
                field_id: FieldId(2),
                value: InspectionValue::F64(min),
            });
            fields.push(InspectionField {
                label: "Y Max".into(),
                field_id: FieldId(3),
                value: InspectionValue::F64(max),
            });
        }
        fields
    }

    fn set_field(&mut self, field_id: FieldId, value: InspectionValue, cx: &mut Context<Self>) {
        match (field_id, value) {
            (FieldId(0), InspectionValue::Component { name }) => {
                // Look up the ComponentId by name from the DB.
                let id = self.db.with_state(|state| {
                    state
                        .component_metadata_iter()
                        .find(|(_, m)| m.name == name)
                        .map(|(id, _)| *id)
                });
                if let Some(id) = id {
                    self.set_component(id, cx);
                }
            }
            (FieldId(1), InspectionValue::Color(c)) => {
                self.color = c;
            }
            (FieldId(2), InspectionValue::F64(v)) => {
                let (_, max) = self.y_bounds.unwrap_or((v, v));
                self.y_bounds = Some((v, max));
            }
            (FieldId(3), InspectionValue::F64(v)) => {
                let (min, _) = self.y_bounds.unwrap_or((v, v));
                self.y_bounds = Some((min, v));
            }
            _ => {}
        }
    }
}
