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
    Bounds, Context, Entity, IntoElement, MouseButton, Pixels, Point, SharedString, Styled, Window,
    canvas, div, point, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::inspect;
#[allow(unused_imports)]
use crate::theme::{ui_px, ui_scale};

use super::plot_common::PanZoom;
use super::plot_common::chrome::{paint_axes, paint_axis_backdrop};
use super::time_series::{
    Colormap, IntensityScale, Override, PADDING, PlotBounds, TimeFormat, X_LABEL_HEIGHT,
    Y_LABEL_WIDTH, format_time_label, format_value_label, plot_area, value_ticks, x_tick_anchor,
    x_ticks,
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
    pub source: crate::data_binding::Binding,
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
    /// Back-reference to the owning plot, set by `SpectrogramPlot::reconcile`,
    /// so the source's inspector can ask the plot to follow a new component.
    #[facet(opaque)]
    pub plot: Option<gpui::WeakEntity<SpectrogramPlot>>,
}

impl SpectrogramTrace {
    pub fn new(component_id: impl Into<ComponentId>, len: usize) -> Self {
        Self {
            source: crate::data_binding::Binding::from(component_id.into()),
            len,
            visible: true,
            label: SharedString::new_static(""),
            colormap: Colormap::default(),
            scale: IntensityScale::default(),
            gain: 1.0,
            plot: None,
        }
    }
}

/// Interactive wrapper around a [`SpectrogramPlot`]: axis chrome, colorbar,
/// hover readout, and pan/zoom input.
pub struct Spectrogram {
    plot: Entity<SpectrogramPlot>,
    pan_zoom: PanZoom,
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
            pan_zoom: PanZoom::default(),
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
                        MouseButton::Right,
                        cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                            if let Some(v) = this.plot.read(cx).effective_view(cx) {
                                crate::temporal::picker::open_plot_actions(
                                    (v.min_x, v.max_x),
                                    event.position,
                                    window,
                                    cx,
                                );
                            }
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                            if event.click_count == 2 {
                                this.reset_view(cx);
                            } else {
                                let view = this.plot.read(cx).effective_view(cx);
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
                                let inside = this
                                    .pan_zoom
                                    .plot_area()
                                    .is_some_and(|pa| pa.contains(&event.position));
                                let next = inside.then_some(event.position);
                                if this.hover != next {
                                    this.hover = next;
                                    cx.notify();
                                }
                                return;
                            }
                            if let Some(view) = this.pan_zoom.drag(event.position) {
                                this.plot
                                    .update(cx, |p, cx| p.set_view_override(Some(view), cx));
                            }
                        },
                    ))
                    .on_scroll_wheel(cx.listener(
                        |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                            let view = this.plot.read(cx).effective_view(cx);
                            if let Some(view) = this.pan_zoom.scroll(view, event) {
                                this.plot
                                    .update(cx, |p, cx| p.set_view_override(Some(view), cx));
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .child(
                        div()
                            .absolute()
                            .left(ui_px(Y_LABEL_WIDTH + PADDING))
                            .top(ui_px(PADDING))
                            .right(ui_px(PADDING))
                            .bottom(ui_px(X_LABEL_HEIGHT + PADDING))
                            .child(self.plot.clone()),
                    )
                    .child(
                        canvas(
                            {
                                let this = cx.entity().downgrade();
                                move |bounds, window, cx| {
                                    let pa = plot_area(bounds, 1, ui_scale(window));
                                    let _ = this.update(cx, |this, _| {
                                        this.pan_zoom.set_plot_area(pa);
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
    let s = ui_scale(window);
    let pa = plot_area(outer, 1, s);
    let theme = crate::theme::theme(cx);
    let view = chrome.view;
    crate::temporal::paint_playhead(pa, (view.min_x, view.max_x), window, cx);

    paint_axis_backdrop(outer, pa, window, cx);

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
            |width, _| point(x - width / 2.0, pa.origin.y + pa.size.height + px(4.0 * s)),
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
            |width, font_size| point(pa.origin.x - width - px(4.0 * s), y - font_size / 2.0),
            window,
            cx,
        );
    }

    paint_axes(pa, theme.axis_color, window);

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
                let gap = px(8.0 * s);
                let x = if pos.x + gap + width > right {
                    pos.x - gap - width
                } else {
                    pos.x + gap
                };
                point(x, pos.y - font_size - px(4.0 * s))
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
    let s = ui_scale(window);
    let width = px(COLORBAR_WIDTH * s);
    let left = pa.origin.x + pa.size.width - width - px(2.0 * s);
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
            pa.origin.y + height - px(crate::views::time_series::LABEL_FONT_SIZE * s),
        ),
    ] {
        crate::views::plot_common::paint_text_label(
            format!("{}{}", format_value_label(value), unit),
            theme.text_secondary,
            |label_width, _| point(left - label_width - px(3.0 * s), y),
            window,
            cx,
        );
    }
}
