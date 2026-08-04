//! Persisted list-plot state.

use std::sync::Arc;

use gpui::{App, Context, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::{ListPlot, ListTrace};
use crate::views::time_series::{Override, PlotStyle};

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ListPlotPanelConfig {
    pub label: String,
    pub traces: Vec<ListTraceConfig>,
    pub custom_title: Override<String>,
    pub x_min_override: Override<f64>,
    pub x_max_override: Override<f64>,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct ListTraceConfig {
    pub component_id: ComponentId,
    pub len: usize,
    pub color: Hsla,
    pub style: PlotStyle,
    pub visible: bool,
    pub label: String,
    pub stroke_width: f32,
}

impl Default for ListTraceConfig {
    fn default() -> Self {
        Self {
            component_id: ComponentId(0),
            len: 0,
            color: Hsla::default(),
            style: PlotStyle::default(),
            visible: true,
            label: String::new(),
            stroke_width: 1.5,
        }
    }
}

impl From<&ListTrace> for ListTraceConfig {
    fn from(trace: &ListTrace) -> Self {
        Self {
            component_id: trace.component_id,
            len: trace.len,
            color: trace.color,
            style: trace.style,
            visible: trace.visible,
            label: trace.label.to_string(),
            stroke_width: trace.stroke_width,
        }
    }
}

impl From<ListTraceConfig> for ListTrace {
    fn from(config: ListTraceConfig) -> Self {
        Self {
            component_id: config.component_id,
            len: config.len,
            color: config.color,
            style: config.style,
            visible: config.visible,
            label: config.label.into(),
            stroke_width: config.stroke_width,
        }
    }
}

impl ListPlot {
    pub fn from_config(config: ListPlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let traces = config.traces.into_iter().map(ListTrace::from).collect();
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

    pub fn to_config(&self, cx: &App) -> ListPlotPanelConfig {
        let line_plot = self.line_plot.read(cx);
        ListPlotPanelConfig {
            label: self.title(cx).to_string(),
            traces: line_plot
                .traces()
                .iter()
                .map(|trace| ListTraceConfig::from(trace.read(cx)))
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
