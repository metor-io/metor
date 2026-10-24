//! List plot. A sibling to the time-series and XY plots that renders the
//! interior of a single sample as `index → value`. Each new sample fully
//! replaces the plotted vector — there is no history.
//!
//! Reuses the GPU pipeline (extended with latest-sample `AxisSource`
//! variants), `PlotBounds`, `Override`, the numeric-axis wrapper it shares
//! with `xy_plot` (`plot_common::NumericPlot`), and the trace-picker
//! scaffolding from `inspector::trace_picker`.

use gpui::{Hsla, SharedString};
use metor_proto::types::ComponentId;

use crate::inspect;

use super::time_series::PlotStyle;

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
    pub source: crate::data_binding::Binding,
    #[facet(skip)]
    pub len: usize,
    pub color: Hsla,
    pub style: PlotStyle,
    pub visible: bool,
    pub label: SharedString,
    #[facet(inspect::range(min = "0.5", max = "10.0"))]
    pub stroke_width: f32,
    /// Back-reference to the owning plot, set by [`ListLinePlot::reconcile`],
    /// so the trace's inspector can ask the plot to follow a new source.
    #[facet(opaque)]
    pub line_plot: Option<gpui::WeakEntity<ListLinePlot>>,
}

impl ListTrace {
    pub fn new(component_id: impl Into<ComponentId>, len: usize, color: Hsla) -> Self {
        Self {
            source: crate::data_binding::Binding::from(component_id.into()),
            len,
            color,
            style: PlotStyle::Line,
            visible: true,
            label: SharedString::new_static(""),
            stroke_width: 1.5,
            line_plot: None,
        }
    }
}

/// Interactive wrapper around a [`ListLinePlot`] that adds axes, legend,
/// and pan/zoom input.
pub type ListPlot = crate::views::plot_common::NumericPlot<ListLinePlot>;
