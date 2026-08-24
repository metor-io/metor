//! Persisted time-series state.
//!
//! Serialization belongs to the view rather than either host that embeds it.

use std::sync::Arc;

use gpui::{App, AppContext, Context, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::Override;
use super::{
    EventOverlay, LinePlot, MeasurementKind, PanelPosition, PlotStyle, TimeFormat, TimeSeriesPlot,
    Trace, YAxis,
};

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PlotPanelConfig {
    pub label: String,
    pub traces: Vec<TraceConfig>,
    pub custom_title: Override<String>,
    pub x_range: String,
    pub axes: Vec<YAxisConfig>,
    pub x_time_format: TimeFormat,
    pub cursors: Vec<MeasurementCursorConfig>,
    pub measurement_panel: MeasurementPanelConfig,
    pub hide_alarm_limits: bool,
    pub hide_alarm_color: bool,
    pub event_overlays: Vec<EventOverlayConfig>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct EventOverlayConfig {
    pub kind: String,
    pub label: String,
    pub visible: bool,
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug)]
pub enum MeasurementPanelConfig {
    #[default]
    Track,
    Pinned {
        x: f32,
        y: f32,
    },
}

impl From<PanelPosition> for MeasurementPanelConfig {
    fn from(position: PanelPosition) -> Self {
        match position {
            PanelPosition::Track => Self::Track,
            PanelPosition::Pinned { x, y } => Self::Pinned { x, y },
        }
    }
}

impl From<MeasurementPanelConfig> for PanelPosition {
    fn from(config: MeasurementPanelConfig) -> Self {
        match config {
            MeasurementPanelConfig::Track => Self::Track,
            MeasurementPanelConfig::Pinned { x, y } => Self::Pinned { x, y },
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct MeasurementCursorConfig {
    pub t_start_us: i64,
    pub t_end_us: i64,
    pub focused_trace_index: i64,
    pub enabled: Vec<MeasurementKind>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct TraceConfig {
    pub component_id: ComponentId,
    pub element_index: usize,
    pub color: Hsla,
    pub style: PlotStyle,
    pub visible: bool,
    pub label: String,
    pub stroke_width: f32,
    pub axis_index: usize,
    /// The text this trace was written as, when it was an expression.
    ///
    /// The component id beside it is where that expression published *last*
    /// session; this is what starts it again, because an expression's
    /// component keeps its history but not its computation across a restart.
    /// Absent for an ordinary component, so layouts saved before this existed
    /// deserialize unchanged and layouts that never use one stay identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            component_id: ComponentId(0),
            element_index: 0,
            color: Hsla::default(),
            style: PlotStyle::default(),
            visible: true,
            label: String::new(),
            stroke_width: 1.5,
            axis_index: 0,
            expression: None,
        }
    }
}

impl From<&Trace> for TraceConfig {
    fn from(trace: &Trace) -> Self {
        Self {
            component_id: trace.component_id,
            element_index: trace.element_index,
            color: trace.color,
            style: trace.style,
            visible: trace.visible,
            label: trace.label.to_string(),
            stroke_width: trace.stroke_width,
            axis_index: trace.axis_index,
            // Filled by `to_config`, which has the database to ask.
            expression: None,
        }
    }
}

impl From<TraceConfig> for Trace {
    fn from(config: TraceConfig) -> Self {
        Self {
            component_id: config.component_id,
            element_index: config.element_index,
            color: config.color,
            style: config.style,
            visible: config.visible,
            label: config.label.into(),
            stroke_width: config.stroke_width,
            axis_index: config.axis_index,
            line_plot: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct YAxisConfig {
    pub label: String,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
    pub color: Override<Hsla>,
}

impl Default for YAxisConfig {
    fn default() -> Self {
        Self {
            label: String::from("Y"),
            y_min_override: Override::Auto,
            y_max_override: Override::Auto,
            color: Override::Auto,
        }
    }
}

impl From<&YAxis> for YAxisConfig {
    fn from(axis: &YAxis) -> Self {
        Self {
            label: axis.label.to_string(),
            y_min_override: axis.y_min_override.clone(),
            y_max_override: axis.y_max_override.clone(),
            color: axis.color.clone(),
        }
    }
}

impl From<YAxisConfig> for YAxis {
    fn from(config: YAxisConfig) -> Self {
        Self {
            label: config.label.into(),
            y_min_override: config.y_min_override,
            y_max_override: config.y_max_override,
            color: config.color,
        }
    }
}

impl TimeSeriesPlot {
    pub fn from_config(config: PlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        // A trace written as an expression is compiled and started before it
        // is built, so what it binds is the component now publishing rather
        // than the one that did last session — which would come back as
        // history with nothing writing into it.
        let traces = config
            .traces
            .into_iter()
            .map(|mut trace| {
                if let Some(text) = &trace.expression
                    && let Ok(bound) = crate::dynamic::expressions::bind(text, &db, cx)
                {
                    trace.component_id = bound.id;
                }
                Trace::from(trace)
            })
            .collect();
        let mut plot = Self::new(db, traces, cx);
        plot.line_plot.update(cx, |line_plot, cx| {
            line_plot.custom_title = config.custom_title.map(SharedString::from);
            if let Ok(range) = config.x_range.parse() {
                line_plot.x_range = Override::Custom(range);
            }
            line_plot.x_time_format = config.x_time_format;
            line_plot.show_alarm_limits = !config.hide_alarm_limits;
            line_plot.show_alarm_color = !config.hide_alarm_color;
            if !config.axes.is_empty() {
                line_plot.axes = config
                    .axes
                    .into_iter()
                    .take(LinePlot::MAX_AXES)
                    .map(|axis| cx.new(|_| YAxis::from(axis)))
                    .collect();
            }
            line_plot.event_overlays = config
                .event_overlays
                .into_iter()
                .filter_map(|overlay| {
                    let key = crate::plot_events::kind_key_from_string(&overlay.kind)?;
                    Some(cx.new(|_| {
                        let mut event = EventOverlay::new(overlay.label, key);
                        event.visible = overlay.visible;
                        event
                    }))
                })
                .collect();
            cx.notify();
        });
        plot.restore_cursors(&config.cursors, cx);
        plot.panel_position = config.measurement_panel.into();
        plot
    }

    pub fn to_config(&self, cx: &App) -> PlotPanelConfig {
        let line_plot = self.line_plot.read(cx);
        let trace_ids: Vec<_> = line_plot
            .traces()
            .iter()
            .map(|trace| trace.entity_id())
            .collect();
        let cursors = self
            .cursors
            .iter()
            .map(|cursor| {
                let cursor = cursor.read(cx);
                MeasurementCursorConfig {
                    t_start_us: cursor.t_start.0,
                    t_end_us: cursor.t_end.0,
                    focused_trace_index: cursor
                        .focused_trace
                        .and_then(|id| trace_ids.iter().position(|trace| *trace == id))
                        .map(|index| index as i64)
                        .unwrap_or(-1),
                    enabled: cursor.enabled.iter().copied().collect(),
                }
            })
            .collect();
        PlotPanelConfig {
            label: self.title(cx).to_string(),
            traces: line_plot
                .traces()
                .iter()
                .map(|trace| {
                    let trace = trace.read(cx);
                    let mut config = TraceConfig::from(trace);
                    config.expression =
                        crate::dynamic::expressions::binding_text(line_plot.db(), trace.component_id);
                    config
                })
                .collect(),
            custom_title: line_plot
                .custom_title
                .as_ref()
                .map(|title| title.to_string()),
            x_range: line_plot
                .x_range
                .as_custom()
                .map(ToString::to_string)
                .unwrap_or_default(),
            axes: line_plot
                .axes
                .iter()
                .map(|axis| YAxisConfig::from(axis.read(cx)))
                .collect(),
            x_time_format: line_plot.x_time_format,
            cursors,
            measurement_panel: self.panel_position.into(),
            hide_alarm_limits: !line_plot.show_alarm_limits,
            hide_alarm_color: !line_plot.show_alarm_color,
            event_overlays: line_plot
                .event_overlays
                .iter()
                .map(|overlay| {
                    let overlay = overlay.read(cx);
                    EventOverlayConfig {
                        kind: crate::plot_events::kind_key_to_string(overlay.key),
                        label: overlay.label.to_string(),
                        visible: overlay.visible,
                    }
                })
                .collect(),
        }
    }
}
