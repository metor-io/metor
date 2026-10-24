//! The interactive wrapper shared by the plots whose axes are both numeric
//! (XY and list): axis chrome, legend, and pan/zoom over an inner GPU line
//! plot that owns every piece of plot state.

use std::sync::Arc;

use gpui::{
    Bounds, Context, Entity, Hsla, IntoElement, MouseButton, Pixels, Render, SharedString, Styled,
    Window, canvas, div, point, prelude::*, px,
};
use metor_db::DB;

use super::PanZoom;
use super::chrome::{paint_axes, paint_axis_backdrop, paint_hline, paint_vline};
use crate::theme::{ui_px, ui_scale};
use crate::views::time_series::{
    PADDING, PlotBounds, X_LABEL_HEIGHT, Y_LABEL_WIDTH, plot_area, value_ticks,
};

/// What [`NumericPlot`] needs from the line-plot entity it wraps.
///
/// The line plots keep their own struct shapes because the inspector reflects
/// their fields; this is just the narrow surface the chrome and gestures use.
pub trait NumericLinePlot: Render + Sized {
    type Trace: 'static;

    fn with_traces(db: Arc<DB>, traces: Vec<Self::Trace>, cx: &mut Context<Self>) -> Self;
    fn effective_view(&self, cx: &gpui::App) -> Option<PlotBounds>;
    fn set_view_override(&mut self, view: Option<PlotBounds>, cx: &mut Context<Self>);
    /// Return every reflected bound override to `Auto`, the double-click reset.
    fn clear_bound_overrides(&mut self);
    fn traces(&self) -> &[Entity<Self::Trace>];
    fn title(&self) -> SharedString;
    /// `(label, color, visible)` for the trace's legend chip.
    fn legend_entry(trace: &Self::Trace) -> (SharedString, Hsla, bool);
    fn toggle_visible(trace: &mut Self::Trace);
}

/// Interactive wrapper around a [`NumericLinePlot`] that adds axes, legend,
/// and pan/zoom input.
///
/// Mirrors `TimeSeriesPlot`: all plot state lives in the inner line plot;
/// this entity owns gesture state and chrome only.
pub struct NumericPlot<P: NumericLinePlot> {
    line_plot: Entity<P>,
    pan_zoom: PanZoom,
}

impl<P: NumericLinePlot> NumericPlot<P> {
    pub fn new(db: Arc<DB>, traces: Vec<P::Trace>, cx: &mut Context<Self>) -> Self {
        let line_plot = cx.new(|cx| P::with_traces(db, traces, cx));
        cx.observe(&line_plot, |_, _, cx| cx.notify()).detach();
        Self {
            line_plot,
            pan_zoom: PanZoom::default(),
        }
    }

    pub fn line_plot(&self) -> &Entity<P> {
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
            lp.clear_bound_overrides();
            lp.set_view_override(None, cx);
            cx.notify();
        });
    }

    fn apply_view(&mut self, view: PlotBounds, cx: &mut Context<Self>) {
        self.line_plot
            .update(cx, |lp, cx| lp.set_view_override(Some(view), cx));
    }
}

impl<P: NumericLinePlot> Render for NumericPlot<P> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        let trace_entities: Vec<Entity<P::Trace>> = self.line_plot.read(cx).traces().to_vec();
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
                                let view = this.line_plot.read(cx).effective_view(cx);
                                this.pan_zoom.press(event.position, view);
                            }
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _event: &gpui::MouseUpEvent, _window, _cx| {
                            this.pan_zoom.release();
                        }),
                    )
                    .on_mouse_move(cx.listener(
                        |this, event: &gpui::MouseMoveEvent, _window, cx| {
                            if !event.dragging() {
                                return;
                            }
                            if let Some(view) = this.pan_zoom.drag(event.position) {
                                this.apply_view(view, cx);
                            }
                        },
                    ))
                    .on_scroll_wheel(cx.listener(
                        |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                            let view = this.line_plot.read(cx).effective_view(cx);
                            if let Some(view) = this.pan_zoom.scroll(view, event) {
                                this.apply_view(view, cx);
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .child(
                        canvas(
                            {
                                let this = cx.entity().downgrade();
                                move |bounds, window, cx| {
                                    let pa = plot_area(bounds, 1, ui_scale(window));
                                    let _ = this.update(cx, |this, _| {
                                        this.pan_zoom.set_plot_area(pa);
                                    });
                                    let lp = underlay_lp.read(cx);
                                    (bounds, lp.effective_view(cx))
                                }
                            },
                            move |_, (bounds, view), window, cx| {
                                if let Some(view) = view {
                                    paint_numeric_underlay(bounds, view, window, cx);
                                }
                            },
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(ui_px(Y_LABEL_WIDTH + PADDING))
                            .top(ui_px(PADDING))
                            .right(ui_px(PADDING))
                            .bottom(ui_px(X_LABEL_HEIGHT + PADDING))
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
                                    paint_numeric_overlay(bounds, view, window, cx);
                                }
                            },
                        )
                        .absolute()
                        .inset_0(),
                    ),
            );

        if show_legend {
            root = root.child(super::plot_legend(
                &trace_entities,
                self.line_plot.clone(),
                ui_px(Y_LABEL_WIDTH + PADDING),
                P::legend_entry,
                P::toggle_visible,
                cx,
            ));
        }

        root
    }
}

/// Paint gridlines and zero lines behind the GPU-rendered plot.
///
/// Both axes are numeric, so X uses [`value_ticks`] just like Y. Zero
/// lines are drawn for whichever axes' ranges cross zero.
fn paint_numeric_underlay(
    outer_bounds: Bounds<Pixels>,
    view: PlotBounds,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let pb = plot_area(outer_bounds, 1, ui_scale(window));
    let theme = crate::theme::theme(cx);

    for tick in value_ticks(view.min_y, view.max_y, 5) {
        let y = view.to_screen(pb, view.min_x, tick).y;
        if y < pb.origin.y || y > pb.origin.y + pb.size.height {
            continue;
        }
        paint_hline(pb, y, px(0.5), theme.grid_color, window);
    }
    for tick in value_ticks(view.min_x, view.max_x, 5) {
        let x = view.to_screen(pb, tick, view.min_y).x;
        if x < pb.origin.x || x > pb.origin.x + pb.size.width {
            continue;
        }
        paint_vline(pb, x, px(0.5), theme.grid_color, window);
    }

    if view.min_y < 0.0 && view.max_y > 0.0 {
        let zero_y = view.to_screen(pb, view.min_x, 0.0).y;
        paint_hline(pb, zero_y, px(1.0), theme.zero_line_color, window);
    }
    if view.min_x < 0.0 && view.max_x > 0.0 {
        let zero_x = view.to_screen(pb, 0.0, view.min_y).x;
        paint_vline(pb, zero_x, px(1.0), theme.zero_line_color, window);
    }
}

/// Paint axis chrome on top of the GPU-rendered plot.
fn paint_numeric_overlay(
    outer_bounds: Bounds<Pixels>,
    view: PlotBounds,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let s = ui_scale(window);
    let pb = plot_area(outer_bounds, 1, s);
    let theme = crate::theme::theme(cx);

    paint_axis_backdrop(outer_bounds, pb, window, cx);

    for tick in value_ticks(view.min_y, view.max_y, 5) {
        let y = view.to_screen(pb, view.min_x, tick).y;
        if y < pb.origin.y || y > pb.origin.y + pb.size.height {
            continue;
        }
        super::paint_value_label(
            tick,
            theme.text_secondary,
            |width, font_size| point(pb.origin.x - width - px(4.0 * s), y - font_size / 2.0),
            window,
            cx,
        );
    }
    for tick in value_ticks(view.min_x, view.max_x, 5) {
        let x = view.to_screen(pb, tick, view.min_y).x;
        if x < pb.origin.x || x > pb.origin.x + pb.size.width {
            continue;
        }
        super::paint_value_label(
            tick,
            theme.text_secondary,
            |width, _| point(x - width / 2.0, pb.origin.y + pb.size.height + px(4.0 * s)),
            window,
            cx,
        );
    }

    paint_axes(pb, theme.axis_color, window);
}
