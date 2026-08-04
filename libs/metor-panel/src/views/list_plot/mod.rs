//! List plot. A sibling to the time-series and XY plots that renders the
//! interior of a single sample as `index → value`. Each new sample fully
//! replaces the plotted vector — there is no history.
//!
//! Reuses the GPU pipeline (extended with latest-sample [`AxisSource`]
//! variants), [`PlotBounds`], [`Override`], the chrome painters from
//! `xy_plot` (since both axes are numeric), and the trace-picker scaffolding
//! from `inspector::trace_picker`.

use std::sync::Arc;

use gpui::{
    Bounds, Context, Entity, Hsla, IntoElement, MouseButton, Pixels, Point, SharedString, Styled,
    Window, canvas, div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::ComponentId;

#[allow(unused_imports)]
use crate::inspect;

use super::time_series::{
    AxisZone, Override, PADDING, PlotBounds, PlotStyle, X_LABEL_HEIGHT, Y_LABEL_WIDTH, axis_zone,
    plot_area,
};
use super::xy_plot::{paint_xy_overlay, paint_xy_underlay};

mod config;
pub use config::{ListPlotPanelConfig, ListTraceConfig};

mod line_plot;
pub use line_plot::ListLinePlot;

pub mod trace_picker;

/// One series on a list plot: a `(component, len)` pair carrying the
/// vector length captured at trace creation. `len` is fixed because
/// component schemas have fixed dimensions in this codebase.
#[derive(Clone, facet::Facet)]
#[facet(pod)]
pub struct ListTrace {
    #[facet(skip)]
    pub component_id: ComponentId,
    #[facet(skip)]
    pub len: usize,
    pub color: Hsla,
    pub style: PlotStyle,
    pub visible: bool,
    pub label: SharedString,
    #[facet(inspect::range(min = "0.5", max = "10.0"))]
    pub stroke_width: f32,
}

impl ListTrace {
    pub fn new(component_id: impl Into<ComponentId>, len: usize, color: Hsla) -> Self {
        Self {
            component_id: component_id.into(),
            len,
            color,
            style: PlotStyle::Line,
            visible: true,
            label: SharedString::new_static(""),
            stroke_width: 1.5,
        }
    }
}

/// Interactive wrapper around a [`ListLinePlot`] that adds axes, legend,
/// and pan/zoom input.
pub struct ListPlot {
    line_plot: Entity<ListLinePlot>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotBounds>,
    drag_zone: AxisZone,
    last_plot_area: Option<Bounds<Pixels>>,
}

impl ListPlot {
    pub fn new(db: Arc<DB>, traces: Vec<ListTrace>, cx: &mut Context<Self>) -> Self {
        let line_plot = cx.new(|cx| {
            let mut lp = ListLinePlot::new(db, cx);
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

    pub fn line_plot(&self) -> &Entity<ListLinePlot> {
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

impl Render for ListPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        let trace_entities: Vec<Entity<ListTrace>> = self.line_plot.read(cx).traces().to_vec();
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
