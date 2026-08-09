//! Persisted XY-plot state.

use std::sync::Arc;

use gpui::{App, Context, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::{XyPlot, XyTrace};
use crate::views::time_series::{Override, PlotStyle};

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct XyPlotPanelConfig {
    pub label: String,
    pub traces: Vec<XyTraceConfig>,
    pub custom_title: Override<String>,
    pub x_min_override: Override<f64>,
    pub x_max_override: Override<f64>,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct XyTraceConfig {
    pub x_component_id: ComponentId,
    pub x_element_index: usize,
    pub y_component_id: ComponentId,
    pub y_element_index: usize,
    pub color: Hsla,
    pub style: PlotStyle,
    pub visible: bool,
    pub label: String,
    pub stroke_width: f32,
}

impl Default for XyTraceConfig {
    fn default() -> Self {
        Self {
            x_component_id: ComponentId(0),
            x_element_index: 0,
            y_component_id: ComponentId(0),
            y_element_index: 0,
            color: Hsla::default(),
            style: PlotStyle::default(),
            visible: true,
            label: String::new(),
            stroke_width: 1.5,
        }
    }
}

impl From<&XyTrace> for XyTraceConfig {
    fn from(trace: &XyTrace) -> Self {
        Self {
            x_component_id: trace.x_component_id,
            x_element_index: trace.x_element_index,
            y_component_id: trace.y_component_id,
            y_element_index: trace.y_element_index,
            color: trace.color,
            style: trace.style,
            visible: trace.visible,
            label: trace.label.to_string(),
            stroke_width: trace.stroke_width,
        }
    }
}

impl From<XyTraceConfig> for XyTrace {
    fn from(config: XyTraceConfig) -> Self {
        Self {
            x_component_id: config.x_component_id,
            x_element_index: config.x_element_index,
            y_component_id: config.y_component_id,
            y_element_index: config.y_element_index,
            color: config.color,
            style: config.style,
            visible: config.visible,
            label: config.label.into(),
            stroke_width: config.stroke_width,
        }
    }
}

impl XyPlot {
    pub fn from_config(config: XyPlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let traces = config.traces.into_iter().map(XyTrace::from).collect();
        let plot = Self::new(db, traces, cx);
        plot.line_plot.update(cx, |line_plot, cx| {
            line_plot.custom_title = config.custom_title.map(SharedString::from);
            line_plot.x_min_override = config.x_min_override;
            line_plot.x_max_override = config.x_max_override;
            line_plot.y_min_override = config.y_min_override;
            line_plot.y_max_override = config.y_max_override;
            cx.notify();
        });
        plot
    }

    pub fn to_config(&self, cx: &App) -> XyPlotPanelConfig {
        let line_plot = self.line_plot.read(cx);
        XyPlotPanelConfig {
            label: self.title(cx).to_string(),
            traces: line_plot
                .traces()
                .iter()
                .map(|trace| XyTraceConfig::from(trace.read(cx)))
                .collect(),
            custom_title: line_plot
                .custom_title
                .as_ref()
                .map(|title| title.to_string()),
            x_min_override: line_plot.x_min_override.clone(),
            x_max_override: line_plot.x_max_override.clone(),
            y_min_override: line_plot.y_min_override.clone(),
            y_max_override: line_plot.y_max_override.clone(),
        }
    }
}
