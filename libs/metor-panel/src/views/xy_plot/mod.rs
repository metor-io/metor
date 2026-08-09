//! XY (phase / correlation) plot. A sibling to the time-series plot that
//! lets each trace pick two `(component, element)` pairs — one driving X,
//! one driving Y — instead of using time as X.
//!
//! Most of the time-series machinery is reused: the GPU pipeline (now
//! generic over X-axis source), `PlotBounds`, `Override`, the per-axis
//! bound scanner [`crate::views::time_series::expand_value_bounds`], and
//! the chrome helpers (axis zones, tick generators, label formatting).
//! Only this module's specifics are new — the trace shape, axis-bound
//! tracker, paint chrome that uses numeric formatting on both axes, and
//! the two-step wizard for picking X then Y.

use std::sync::Arc;

use gpui::{
    Bounds, Context, Entity, Hsla, IntoElement, MouseButton, PathBuilder, Pixels, Point,
    SharedString, Styled, Window, canvas, div, point, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::ComponentId;

#[allow(unused_imports)]
use crate::inspect;

use super::time_series::{
    AxisZone, Override, PADDING, PlotBounds, PlotStyle, X_LABEL_HEIGHT, Y_LABEL_WIDTH, axis_zone,
    plot_area, value_ticks,
};

mod config;
pub use config::{XyPlotPanelConfig, XyTraceConfig};

mod line_plot;
pub use line_plot::XyLinePlot;

pub mod trace_picker;

/// One series on an XY plot: a `(component, element)` for each axis,
/// with its own color, style, and label.
///
/// `Bar` is excluded from the inspector for XY (the wizard defaults to
/// `Scatter`); the GPU layer still renders it correctly if a serialized
/// config carries `Bar` in.
#[derive(Clone, facet::Facet)]
#[facet(pod)]
pub struct XyTrace {
    #[facet(skip)]
    pub x_component_id: ComponentId,
    #[facet(skip)]
    pub x_element_index: usize,
    #[facet(skip)]
    pub y_component_id: ComponentId,
    #[facet(skip)]
    pub y_element_index: usize,
    pub color: Hsla,
    #[facet(inspect::variants = "Line,Scatter")]
    pub style: PlotStyle,
    pub visible: bool,
    pub label: SharedString,
    #[facet(inspect::range(min = "0.5", max = "10.0"))]
    pub stroke_width: f32,
}

impl XyTrace {
    pub fn new(
        x_component_id: impl Into<ComponentId>,
        x_element_index: usize,
        y_component_id: impl Into<ComponentId>,
        y_element_index: usize,
        color: Hsla,
    ) -> Self {
        Self {
            x_component_id: x_component_id.into(),
            x_element_index,
            y_component_id: y_component_id.into(),
            y_element_index,
            color,
            style: PlotStyle::Scatter,
            visible: true,
            label: SharedString::new_static(""),
            stroke_width: 1.5,
        }
    }
}

/// Interactive wrapper around an [`XyLinePlot`] that adds axes, legend,
/// and pan/zoom input.
///
/// Mirrors `TimeSeriesPlot`: all plot state lives in the inner
/// [`XyLinePlot`]; this entity owns drag state and chrome only.
pub struct XyPlot {
    line_plot: Entity<XyLinePlot>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotBounds>,
    drag_zone: AxisZone,
    last_plot_area: Option<Bounds<Pixels>>,
}

impl XyPlot {
    pub fn new(db: Arc<DB>, traces: Vec<XyTrace>, cx: &mut Context<Self>) -> Self {
        let line_plot = cx.new(|cx| {
            let mut lp = XyLinePlot::new(db, cx);
            lp.bind_traces(traces, cx);
            lp
        });
        cx.observe(&line_plot, |_, _, cx| cx.notify()).detach();
        Self {
            line_plot,
            drag_start: None,
            drag_start_view: None,
            drag_zone: AxisZone::Plot,
            last_plot_area: None,
        }
    }

    pub fn line_plot(&self) -> &Entity<XyLinePlot> {
        &self.line_plot
    }

    pub fn title(&self, cx: &gpui::App) -> SharedString {
        self.line_plot.read(cx).title()
    }

    pub fn view(&self, cx: &gpui::App) -> Option<PlotBounds> {
        self.line_plot.read(cx).effective_view(cx)
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.line_plot.update(cx, |lp, cx| {
            lp.x_min_override = Override::Auto;
            lp.x_max_override = Override::Auto;
            lp.y_min_override = Override::Auto;
            lp.y_max_override = Override::Auto;
            lp.set_view_override(None, cx);
            cx.notify();
        });
    }
}

impl Render for XyPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        let trace_entities: Vec<Entity<XyTrace>> = self.line_plot.read(cx).traces().to_vec();
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
                                    .map(|pa| axis_zone(event.position, pa, 1))
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
                                AxisZone::YAxis(_) => start_view.offset_y(ny),
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

                            let zone = axis_zone(event.position, pa, 1);
                            let (ax, ay) = view.screen_anchor(pa, event.position);
                            let new_view = match zone {
                                AxisZone::Plot => view.zoom_at(factor, ax, 1.0 - ay),
                                AxisZone::XAxis => view.zoom_x(factor, ax),
                                AxisZone::YAxis(_) => view.zoom_y(factor, 1.0 - ay),
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
                                        this.last_plot_area = Some(plot_area(bounds, 1));
                                    });
                                    let lp = underlay_lp.read(cx);
                                    (bounds, lp.effective_view(cx))
                                }
                            },
                            move |_, (bounds, view), window, cx| {
                                if let Some(view) = view {
                                    paint_xy_underlay(bounds, view, window, cx);
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
                                (bounds, lp.effective_view(cx))
                            },
                            move |_, (bounds, view), window, cx| {
                                if let Some(view) = view {
                                    paint_xy_overlay(bounds, view, window, cx);
                                }
                            },
                        )
                        .absolute()
                        .inset_0(),
                    ),
            );

        if show_legend {
            root = root.child(crate::views::plot_common::plot_legend(
                &trace_entities,
                self.line_plot.clone(),
                px(Y_LABEL_WIDTH + PADDING),
                |trace| (trace.label.clone(), trace.color, trace.visible),
                |trace| trace.visible = !trace.visible,
                cx,
            ));
        }

        root
    }
}

/// Paint gridlines and zero lines behind the GPU-rendered XY plot.
///
/// Both axes are numeric, so X uses [`value_ticks`] just like Y. Zero
/// lines are drawn for whichever axes' ranges cross zero.
pub(crate) fn paint_xy_underlay(
    outer_bounds: Bounds<Pixels>,
    view: PlotBounds,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let pb = plot_area(outer_bounds, 1);
    let theme = crate::theme::theme(cx);

    for tick in value_ticks(view.min_y, view.max_y, 5) {
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
    for tick in value_ticks(view.min_x, view.max_x, 5) {
        let x = view.to_screen(pb, tick, view.min_y).x;
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
    if view.min_x < 0.0 && view.max_x > 0.0 {
        let zero_x = view.to_screen(pb, 0.0, view.min_y).x;
        let mut zp = PathBuilder::stroke(px(1.0));
        zp.move_to(point(zero_x, pb.origin.y));
        zp.line_to(point(zero_x, pb.origin.y + pb.size.height));
        if let Ok(path) = zp.build() {
            window.paint_path(path, theme.zero_line_color);
        }
    }
}

/// Paint axis chrome on top of the GPU-rendered XY plot.
pub(crate) fn paint_xy_overlay(
    outer_bounds: Bounds<Pixels>,
    view: PlotBounds,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let pb = plot_area(outer_bounds, 1);
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

    for tick in value_ticks(view.min_y, view.max_y, 5) {
        let y = view.to_screen(pb, view.min_x, tick).y;
        if y < pb.origin.y || y > pb.origin.y + pb.size.height {
            continue;
        }
        crate::views::plot_common::paint_value_label(
            tick,
            theme.text_secondary,
            |width, font_size| point(pb.origin.x - width - px(4.0), y - font_size / 2.0),
            window,
            cx,
        );
    }
    for tick in value_ticks(view.min_x, view.max_x, 5) {
        let x = view.to_screen(pb, tick, view.min_y).x;
        if x < pb.origin.x || x > pb.origin.x + pb.size.width {
            continue;
        }
        crate::views::plot_common::paint_value_label(
            tick,
            theme.text_secondary,
            |width, _| point(x - width / 2.0, pb.origin.y + pb.size.height + px(4.0)),
            window,
            cx,
        );
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
