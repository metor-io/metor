//! Spectrogram / waterfall. A third standalone plot wrapper beside the
//! time-series and list plots: it renders a vector component's history as a
//! time × bin raster with magnitude as colour, which is the only way to see
//! how a spectrum *moves* — a list plot shows one instant and a line plot one
//! bin.
//!
//! The source is normally an expression, `= fft(window(x, N))`, whose output
//! is `N/2 + 1` one-sided magnitudes. Nothing in the DB carries frequency
//! metadata, so the Y axis is the bin index unless `sample_rate` is set, at
//! which point the same axis is relabelled in Hz.
//!
//! Reuses [`PlotBounds`], [`Override`], the GPU render state and its new
//! intensity-field pass, and the trace-picker scaffolding; the pan/zoom
//! gestures are the list plot's, with X in microseconds.

use std::sync::Arc;

use gpui::{
    Bounds, Context, Entity, IntoElement, MouseButton, PathBuilder, Pixels, Point, SharedString,
    Styled, Window, canvas, div, point, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::ComponentId;

#[allow(unused_imports)]
use crate::inspect;

use super::time_series::{
    AxisZone, Colormap, IntensityScale, Override, PADDING, PlotBounds, TimeFormat, X_LABEL_HEIGHT,
    Y_LABEL_WIDTH, axis_zone, format_time_label, format_value_label, plot_area, value_ticks,
    x_tick_anchor, x_ticks,
};

pub(crate) mod grid;

mod config;
pub use config::{SpectrogramPanelConfig, SpectrogramTraceConfig};

mod plot;
pub use plot::SpectrogramPlot;

pub mod trace_picker;

/// Width of the colorbar strip painted inside the plot's right edge.
const COLORBAR_WIDTH: f32 = 12.0;
/// Quads stacked to draw the colorbar. Enough that the ramp reads as
/// continuous at any pane height without paying for a per-pixel gradient.
const COLORBAR_STEPS: usize = 32;

/// One source on a spectrogram: a `(component, len)` pair carrying the bin
/// count captured when the source was picked.
///
/// There is no colour field — the colormap *is* the colour, and a second
/// colour would only confuse which one the image is showing.
#[derive(Clone, facet::Facet)]
#[facet(pod)]
pub struct SpectrogramTrace {
    #[facet(skip)]
    pub component_id: ComponentId,
    #[facet(skip)]
    pub len: usize,
    pub visible: bool,
    pub label: SharedString,
    pub colormap: Colormap,
    pub scale: IntensityScale,
    /// Multiplies normalized intensity before the colour lookup, so a faint
    /// band can be pushed into the visible part of the ramp.
    #[facet(inspect::range(min = "0.1", max = "100.0"))]
    pub gain: f32,
    /// The `=` expression this source plots, when it is one; the source's
    /// share is what keeps it computing.
    #[facet(opaque)]
    pub expression: Option<crate::dynamic::expressions::Expression>,
    /// Back-reference to the owning plot, set by `SpectrogramPlot::reconcile`,
    /// so the source's inspector can ask the plot to follow a new component.
    #[facet(opaque)]
    pub plot: Option<gpui::WeakEntity<SpectrogramPlot>>,
}

impl SpectrogramTrace {
    pub fn new(component_id: impl Into<ComponentId>, len: usize) -> Self {
        Self {
            component_id: component_id.into(),
            len,
            visible: true,
            label: SharedString::new_static(""),
            colormap: Colormap::default(),
            scale: IntensityScale::default(),
            gain: 1.0,
            expression: None,
            plot: None,
        }
    }
}

/// Interactive wrapper around a [`SpectrogramPlot`]: axis chrome, colorbar,
/// hover readout, and pan/zoom input.
pub struct Spectrogram {
    plot: Entity<SpectrogramPlot>,
    drag_start: Option<Point<Pixels>>,
    drag_start_view: Option<PlotBounds>,
    drag_zone: AxisZone,
    last_plot_area: Option<Bounds<Pixels>>,
    /// Pointer position while bare-hovering the plot, driving the readout.
    hover: Option<Point<Pixels>>,
}

impl Spectrogram {
    pub fn new(db: Arc<DB>, traces: Vec<SpectrogramTrace>, cx: &mut Context<Self>) -> Self {
        let plot = cx.new(|cx| {
            let mut p = SpectrogramPlot::new(db, cx);
            p.bind_traces(traces, cx);
            p
        });
        cx.observe(&plot, |_, _, cx| cx.notify()).detach();
        Self {
            plot,
            drag_start: None,
            drag_start_view: None,
            drag_zone: AxisZone::Plot,
            last_plot_area: None,
            hover: None,
        }
    }

    pub fn plot(&self) -> &Entity<SpectrogramPlot> {
        &self.plot
    }

    pub fn title(&self, cx: &gpui::App) -> SharedString {
        self.plot.read(cx).title()
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.plot.update(cx, |p, cx| {
            p.y_min_override = Override::Auto;
            p.y_max_override = Override::Auto;
            p.set_view_override(None, cx);
            cx.notify();
        });
    }

    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if self.hover.take().is_some() {
            cx.notify();
        }
    }
}

impl Render for Spectrogram {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        let overlay_plot = self.plot.clone();
        let hover = self.hover;

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_secondary)
            .child(
                div()
                    .id(("spectrogram", cx.entity().entity_id().as_u64() as usize))
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                        if !*hovered {
                            this.clear_hover(cx);
                        }
                    }))
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
                                this.drag_start_view = this.plot.read(cx).effective_view(cx);
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
                                let inside = this
                                    .last_plot_area
                                    .is_some_and(|pa| pa.contains(&event.position));
                                let next = inside.then_some(event.position);
                                if this.hover != next {
                                    this.hover = next;
                                    cx.notify();
                                }
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
                            this.plot
                                .update(cx, |p, cx| p.set_view_override(Some(new_view), cx));
                        },
                    ))
                    .on_scroll_wheel(cx.listener(
                        |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                            let Some(view) = this.plot.read(cx).effective_view(cx) else {
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
                            this.plot
                                .update(cx, |p, cx| p.set_view_override(Some(new_view), cx));
                            cx.stop_propagation();
                        },
                    ))
                    .child(
                        div()
                            .absolute()
                            .left(px(Y_LABEL_WIDTH + PADDING))
                            .top(px(PADDING))
                            .right(px(PADDING))
                            .bottom(px(X_LABEL_HEIGHT + PADDING))
                            .child(self.plot.clone()),
                    )
                    .child(
                        canvas(
                            {
                                let this = cx.entity().downgrade();
                                move |bounds, _window, cx| {
                                    let pa = plot_area(bounds, 1);
                                    let _ = this.update(cx, |this, _| {
                                        this.last_plot_area = Some(pa);
                                    });
                                    let chrome = Chrome::capture(&overlay_plot, pa, hover, cx);
                                    (bounds, chrome)
                                }
                            },
                            move |_,
                                  (bounds, chrome): (Bounds<Pixels>, Option<Chrome>),
                                  window,
                                  cx| {
                                if let Some(chrome) = chrome {
                                    paint_chrome(bounds, &chrome, window, cx);
                                }
                            },
                        )
                        .absolute()
                        .inset_0(),
                    ),
            )
    }
}

/// Everything the chrome painter needs, read out of the entity tree during
/// prepaint so paint itself only draws.
struct Chrome {
    view: PlotBounds,
    data_start: f64,
    time_format: TimeFormat,
    /// Bins per Hz mapping, when the operator supplied a sample rate.
    hz_per_bin: Option<f64>,
    scale: IntensityScale,
    colormap: Colormap,
    intensity: (f64, f64),
    show_colorbar: bool,
    readout: Option<(Point<Pixels>, SharedString)>,
}

impl Chrome {
    fn capture(
        plot: &Entity<SpectrogramPlot>,
        pa: Bounds<Pixels>,
        hover: Option<Point<Pixels>>,
        cx: &gpui::App,
    ) -> Option<Self> {
        let p = plot.read(cx);
        let view = p.effective_view(cx)?;
        let len = p.bin_count(cx);
        // fft's one-sided output puts Nyquist in the last bin, so the top of
        // the axis is rate/2 and the step follows from the bin count.
        let hz_per_bin = p
            .sample_rate
            .as_custom()
            .copied()
            .filter(|rate| *rate > 0.0 && len > 1)
            .map(|rate| (rate / 2.0) / (len - 1) as f64);
        let scale = p.scale(cx);
        let intensity = p.intensity_range();
        let data_start = p.data_start().unwrap_or(view.min_x);

        let readout = hover.and_then(|pos| {
            let frac_x = f32::from(pos.x - pa.origin.x) / f32::from(pa.size.width);
            let frac_y = 1.0 - f32::from(pos.y - pa.origin.y) / f32::from(pa.size.height);
            let sample = p.sample_at(frac_x, frac_y, cx)?;
            let axis = match hz_per_bin {
                Some(step) => format_frequency(sample.bin as f64 * step),
                None => format!("bin {}", sample.bin),
            };
            let text = format!(
                "{} · {} · {}{}",
                format_time_label(sample.ts, data_start as i64, p.x_time_format, view.width()),
                axis,
                format_value_label(sample.value as f64),
                scale.unit(),
            );
            Some((pos, SharedString::from(text)))
        });

        Some(Self {
            view,
            data_start,
            time_format: p.x_time_format,
            hz_per_bin,
            scale,
            colormap: p.colormap(cx),
            intensity,
            show_colorbar: p.show_colorbar,
            readout,
        })
    }
}

/// Frequency axis label: kHz once the value earns it, otherwise Hz.
fn format_frequency(hz: f64) -> String {
    if hz.abs() >= 1000.0 {
        format!("{:.1} kHz", hz / 1000.0)
    } else {
        format!("{:.0} Hz", hz)
    }
}

/// Paint the axis chrome over the GPU-rendered field.
///
/// No interior gridlines: over a filled image they read as artefacts of the
/// data rather than as chrome.
fn paint_chrome(outer: Bounds<Pixels>, chrome: &Chrome, window: &mut Window, cx: &mut gpui::App) {
    let pa = plot_area(outer, 1);
    let theme = crate::theme::theme(cx);
    let view = chrome.view;

    let axis_bg = theme.plot_chrome_bg();
    window.paint_quad(gpui::fill(
        Bounds {
            origin: outer.origin,
            size: gpui::Size {
                width: pa.origin.x - outer.origin.x,
                height: outer.size.height,
            },
        },
        axis_bg,
    ));
    window.paint_quad(gpui::fill(
        Bounds {
            origin: point(pa.origin.x, pa.origin.y + pa.size.height),
            size: gpui::Size {
                width: pa.size.width,
                height: outer.origin.y + outer.size.height - pa.origin.y - pa.size.height,
            },
        },
        axis_bg,
    ));

    let anchor = x_tick_anchor(chrome.time_format, chrome.data_start);
    for tick in x_ticks(&view, 6, anchor) {
        let x = view.to_screen(pa, tick as f64, view.min_y).x;
        if x < pa.origin.x || x > pa.origin.x + pa.size.width {
            continue;
        }
        let label = format_time_label(
            tick,
            chrome.data_start as i64,
            chrome.time_format,
            view.width(),
        );
        crate::views::plot_common::paint_text_label(
            label,
            theme.text_secondary,
            |width, _| point(x - width / 2.0, pa.origin.y + pa.size.height + px(4.0)),
            window,
            cx,
        );
    }

    for tick in value_ticks(view.min_y, view.max_y, 5) {
        let y = view.to_screen(pa, view.min_x, tick).y;
        if y < pa.origin.y || y > pa.origin.y + pa.size.height {
            continue;
        }
        let label = match chrome.hz_per_bin {
            Some(step) => format_frequency(tick * step),
            None => format_value_label(tick),
        };
        crate::views::plot_common::paint_text_label(
            label,
            theme.text_secondary,
            |width, font_size| point(pa.origin.x - width - px(4.0), y - font_size / 2.0),
            window,
            cx,
        );
    }

    let mut axes = PathBuilder::stroke(px(1.0));
    axes.move_to(point(pa.origin.x, pa.origin.y));
    axes.line_to(point(pa.origin.x, pa.origin.y + pa.size.height));
    axes.line_to(point(
        pa.origin.x + pa.size.width,
        pa.origin.y + pa.size.height,
    ));
    if let Ok(path) = axes.build() {
        window.paint_path(path, theme.axis_color);
    }

    if chrome.show_colorbar {
        paint_colorbar(pa, chrome, window, cx);
    }

    if let Some((pos, text)) = &chrome.readout {
        crate::views::plot_common::paint_text_label(
            text.clone(),
            theme.text_primary,
            |width, font_size| {
                // Flip to the pointer's left near the right edge so the
                // readout never runs off the pane.
                let right = pa.origin.x + pa.size.width;
                let x = if pos.x + px(8.0) + width > right {
                    pos.x - px(8.0) - width
                } else {
                    pos.x + px(8.0)
                };
                point(x, pos.y - font_size - px(4.0))
            },
            window,
            cx,
        );
    }
}

/// A vertical ramp inside the plot's right edge, labelled with the intensity
/// range the colours currently map. Without it the picture has no units.
fn paint_colorbar(pa: Bounds<Pixels>, chrome: &Chrome, window: &mut Window, cx: &mut gpui::App) {
    let theme = crate::theme::theme(cx);
    let width = px(COLORBAR_WIDTH);
    let left = pa.origin.x + pa.size.width - width - px(2.0);
    let height = pa.size.height;
    let step = height / COLORBAR_STEPS as f32;
    for i in 0..COLORBAR_STEPS {
        let t = 1.0 - (i as f32 + 0.5) / COLORBAR_STEPS as f32;
        let color = chrome.colormap.sample(&theme, theme.line_color, t);
        window.paint_quad(gpui::fill(
            Bounds {
                origin: point(left, pa.origin.y + step * i as f32),
                // Overlap by a hair so rounding cannot leave seams between
                // the stacked quads.
                size: gpui::Size {
                    width,
                    height: step + px(1.0),
                },
            },
            color,
        ));
    }

    let (lo, hi) = chrome.intensity;
    let unit = chrome.scale.unit();
    for (value, y) in [
        (hi, pa.origin.y),
        (
            lo,
            pa.origin.y + height - px(crate::views::time_series::LABEL_FONT_SIZE),
        ),
    ] {
        crate::views::plot_common::paint_text_label(
            format!("{}{}", format_value_label(value), unit),
            theme.text_secondary,
            |label_width, _| point(left - label_width - px(3.0), y),
            window,
            cx,
        );
    }
}
