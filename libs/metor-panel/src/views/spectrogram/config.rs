//! Persisted spectrogram state.

use std::sync::Arc;

use gpui::{App, Context, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::{Spectrogram, SpectrogramTrace};
use crate::views::time_series::{Colormap, IntensityScale, Override, TimeFormat};

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct SpectrogramPanelConfig {
    pub label: String,
    pub traces: Vec<SpectrogramTraceConfig>,
    pub custom_title: Override<String>,
    /// [`TimeRangeBehavior`](crate::views::time_series::TimeRangeBehavior) as
    /// text; empty means follow the app-wide range.
    pub x_range: String,
    pub x_time_format: TimeFormat,
    /// Visible bin range. Bins, not Hz — `sample_rate` only relabels the axis.
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
    /// Pins the colour mapping's ends, in the scale's display units (dB under
    /// `Log`), against the per-frame auto range.
    pub intensity_min: Override<f64>,
    pub intensity_max: Override<f64>,
    /// Sampling rate of the signal the spectrum came from, in Hz. With it the
    /// Y axis is labelled in Hz; without it, in bin indices.
    pub sample_rate: Override<f64>,
    pub show_colorbar: bool,
}

/// The colorbar is on by default: without a legend the colours are just
/// pretty, so a fresh pane has to explain itself.
impl Default for SpectrogramPanelConfig {
    fn default() -> Self {
        Self {
            label: String::new(),
            traces: Vec::new(),
            custom_title: Override::Auto,
            x_range: String::new(),
            x_time_format: TimeFormat::default(),
            y_min_override: Override::Auto,
            y_max_override: Override::Auto,
            intensity_min: Override::Auto,
            intensity_max: Override::Auto,
            sample_rate: Override::Auto,
            show_colorbar: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct SpectrogramTraceConfig {
    pub component_id: ComponentId,
    pub len: usize,
    pub visible: bool,
    pub label: String,
    pub colormap: Colormap,
    pub scale: IntensityScale,
    pub gain: f32,
    /// The text this source was written as, when it was an expression — the
    /// usual case here, since a spectrum is normally `= fft(window(x, N))`.
    /// The component id beside it is where that expression published *last*
    /// session; this is what starts it again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
}

impl Default for SpectrogramTraceConfig {
    fn default() -> Self {
        Self {
            component_id: ComponentId(0),
            len: 0,
            visible: true,
            label: String::new(),
            colormap: Colormap::default(),
            scale: IntensityScale::default(),
            gain: 1.0,
            expression: None,
        }
    }
}

impl From<&SpectrogramTrace> for SpectrogramTraceConfig {
    fn from(trace: &SpectrogramTrace) -> Self {
        Self {
            component_id: trace.source.id(),
            len: trace.len,
            visible: trace.visible,
            label: trace.label.to_string(),
            colormap: trace.colormap,
            scale: trace.scale,
            gain: trace.gain,
            // Filled by `to_config`, which has the database to ask.
            expression: trace.source.expression_text(),
        }
    }
}

impl From<SpectrogramTraceConfig> for SpectrogramTrace {
    fn from(config: SpectrogramTraceConfig) -> Self {
        Self {
            source: crate::data_binding::Binding::unresolved(
                config.component_id,
                config.expression,
            ),
            len: config.len,
            visible: config.visible,
            label: config.label.into(),
            colormap: config.colormap,
            scale: config.scale,
            gain: config.gain,
            plot: None,
        }
    }
}

impl Spectrogram {
    pub fn from_config(
        config: SpectrogramPanelConfig,
        db: Arc<DB>,
        cx: &mut Context<Self>,
    ) -> Self {
        // A source written as an expression is compiled and started before it
        // is built, so what it binds is the component now publishing rather
        // than the one that did last session.
        let traces = config
            .traces
            .into_iter()
            .map(|config| {
                let mut trace = SpectrogramTrace::from(config);
                trace.source.resolve(&db, cx);
                trace
            })
            .collect();
        let plot = Self::new(db, traces, cx);
        plot.plot.update(cx, |plot, cx| {
            plot.custom_title = config.custom_title.map(SharedString::from);
            if let Ok(range) = config.x_range.parse() {
                plot.x_range = Override::Custom(range);
            }
            plot.x_time_format = config.x_time_format;
            plot.y_min_override = config.y_min_override;
            plot.y_max_override = config.y_max_override;
            plot.intensity_min = config.intensity_min;
            plot.intensity_max = config.intensity_max;
            plot.sample_rate = config.sample_rate;
            plot.show_colorbar = config.show_colorbar;
            cx.notify();
        });
        plot
    }

    pub fn to_config(&self, cx: &App) -> SpectrogramPanelConfig {
        let plot = self.plot.read(cx);
        SpectrogramPanelConfig {
            label: self.title(cx).to_string(),
            traces: plot
                .traces()
                .iter()
                .map(|trace| SpectrogramTraceConfig::from(trace.read(cx)))
                .collect(),
            custom_title: plot.custom_title.as_ref().map(|title| title.to_string()),
            x_range: plot
                .x_range
                .as_custom()
                .map(ToString::to_string)
                .unwrap_or_default(),
            x_time_format: plot.x_time_format,
            y_min_override: plot.y_min_override.clone(),
            y_max_override: plot.y_max_override.clone(),
            intensity_min: plot.intensity_min.clone(),
            intensity_max: plot.intensity_max.clone(),
            sample_rate: plot.sample_rate.clone(),
            show_colorbar: plot.show_colorbar,
        }
    }
}
