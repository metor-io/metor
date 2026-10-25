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

use gpui::{Hsla, SharedString};
use metor_proto::types::ComponentId;

use crate::inspect;

use super::time_series::PlotStyle;

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
    pub x_source: crate::data_binding::Binding,
    pub x_element_index: usize,
    pub y_source: crate::data_binding::Binding,
    pub y_element_index: usize,
    pub color: Hsla,
    #[facet(inspect::variants = "Line,Scatter")]
    pub style: PlotStyle,
    pub visible: bool,
    pub label: SharedString,
    #[facet(inspect::range(min = "0.5", max = "10.0"))]
    pub stroke_width: f32,
    /// Back-reference to the owning plot, set by [`XyLinePlot::reconcile`],
    /// so the trace's inspector can ask the plot to follow a new source.
    #[facet(opaque)]
    pub line_plot: Option<gpui::WeakEntity<XyLinePlot>>,
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
            x_source: crate::data_binding::Binding::from(x_component_id.into()),
            x_element_index,
            y_source: crate::data_binding::Binding::from(y_component_id.into()),
            y_element_index,
            color,
            style: PlotStyle::Scatter,
            visible: true,
            label: SharedString::new_static(""),
            stroke_width: 1.5,
            line_plot: None,
        }
    }
}

/// Interactive wrapper around an [`XyLinePlot`] that adds axes, legend,
/// and pan/zoom input.
pub type XyPlot = crate::views::plot_common::NumericPlot<XyLinePlot>;
