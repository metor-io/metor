use std::sync::Arc;

use gpui::{
    App, Context, Entity, Hsla, IntoElement, Render, SharedString, Window, div, prelude::*,
};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};
use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorCallback};
use crate::views::dashboard::DashboardPanel;
use crate::views::list_plot::{ListLinePlot, ListPlot, ListTrace};
use crate::views::time_series::{LinePlot, Override, PlotStyle, Trace};
use crate::views::viewer_3d::Viewer3d;
use crate::views::xy_plot::{XyLinePlot, XyPlot, XyTrace};
use crate::views::{
    ComponentBrowser, ComponentTable, ComponentText, DataTable, TimeSeriesPlot, TrafficLight,
    TrafficLightGrid, new_component_browser, new_component_table, new_data_table,
};

use super::item::{PaneItem, PaneItemHandle};
use super::pane::Pane;

/// Persisted shape of a [`TextPanel`].
#[derive(facet::Facet, Default)]
pub struct TextPanelConfig {
    /// Display label and serialization key for the source component.
    pub component: String,
}

/// Pane item that renders a single component's latest value as text.
pub struct TextPanel {
    inner: Entity<ComponentText>,
    label: SharedString,
}

impl TextPanel {
    pub fn new(
        db: Arc<DB>,
        component_id: ComponentId,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let inner = cx.new(|cx| ComponentText::new(db, component_id, cx));
        Self {
            inner,
            label: label.into(),
        }
    }

    pub fn from_config(cfg: TextPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let component_id = ComponentId::new(&cfg.component);
        Self::new(db, component_id, cfg.component, cx)
    }
}

impl Render for TextPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for TextPanel {
    type Config = TextPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "component_text"
    }

    fn to_config(&self, _cx: &App) -> TextPanelConfig {
        TextPanelConfig {
            component: self.label.to_string(),
        }
    }
}

/// Persisted shape of a [`TrafficLightPanel`].
#[derive(facet::Facet, Clone, Default)]
pub struct TrafficLightPanelConfig {
    pub component: String,
    pub color: Option<Hsla>,
}

/// Pane item rendering one component as a coloured on/off square.
pub struct TrafficLightPanel {
    inner: Entity<TrafficLight>,
    label: SharedString,
}

impl TrafficLightPanel {
    pub fn new(
        db: Arc<DB>,
        component_id: ComponentId,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let inner = cx.new(|cx| TrafficLight::new(db, component_id, cx));
        Self {
            inner,
            label: label.into(),
        }
    }

    pub fn from_config(cfg: TrafficLightPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let component_id = ComponentId::new(&cfg.component);
        let inner = cx.new(|cx| TrafficLight::new(db, component_id, cx));
        if let Some(color) = cfg.color {
            inner.update(cx, |t, cx| t.set_color(color, cx));
        }
        Self {
            inner,
            label: cfg.component.into(),
        }
    }
}

impl Render for TrafficLightPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for TrafficLightPanel {
    type Config = TrafficLightPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "traffic_light"
    }

    fn to_config(&self, cx: &App) -> TrafficLightPanelConfig {
        TrafficLightPanelConfig {
            component: self.label.to_string(),
            color: Some(self.inner.read(cx).color()),
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

/// Persisted shape of a [`TrafficLightGridPanel`].
#[derive(facet::Facet, Clone, Default)]
pub struct TrafficLightGridPanelConfig {
    pub pattern: String,
    pub color: Option<Hsla>,
}

/// Pane item rendering every component matching a glob pattern as a grid of
/// traffic-light tiles.
pub struct TrafficLightGridPanel {
    inner: Entity<TrafficLightGrid>,
    label: SharedString,
}

impl TrafficLightGridPanel {
    pub fn new(db: Arc<DB>, pattern: SharedString, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| TrafficLightGrid::new(db, pattern, cx));
        Self {
            inner,
            label: "Traffic Lights".into(),
        }
    }

    pub fn from_config(
        cfg: TrafficLightGridPanelConfig,
        db: Arc<DB>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pattern = SharedString::from(cfg.pattern);
        let inner = cx.new(|cx| TrafficLightGrid::new(db, pattern, cx));
        if let Some(color) = cfg.color {
            inner.update(cx, |g, cx| g.set_color(color, cx));
        }
        Self {
            inner,
            label: "Traffic Lights".into(),
        }
    }
}

impl Render for TrafficLightGridPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for TrafficLightGridPanel {
    type Config = TrafficLightGridPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "traffic_light_grid"
    }

    fn to_config(&self, cx: &App) -> TrafficLightGridPanelConfig {
        let inner = self.inner.read(cx);
        TrafficLightGridPanelConfig {
            pattern: inner.pattern().to_string(),
            color: Some(inner.color()),
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

/// Persisted shape of a [`TablePanel`]. Currently empty — the panel renders
/// every component in the DB and has no per-instance configuration.
#[derive(facet::Facet, Default)]
pub struct TablePanelConfig {}

/// Pane item listing every component in the DB as a flat table.
pub struct TablePanel {
    inner: Entity<ComponentTable>,
    label: SharedString,
}

impl TablePanel {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| new_component_table(db, cx));
        Self {
            inner,
            label: "Components".into(),
        }
    }

    pub fn from_config(_cfg: TablePanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::new(db, cx)
    }
}

impl Render for TablePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for TablePanel {
    type Config = TablePanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "component_table"
    }

    fn to_config(&self, _cx: &App) -> TablePanelConfig {
        TablePanelConfig {}
    }
}

/// Persisted shape of a [`DataTablePanel`]. No per-instance configuration today.
#[derive(facet::Facet, Default)]
pub struct DataTablePanelConfig {}

/// Pane item rendering one row per component, grouped by namespace, with
/// live values per element. Wraps the same [`DataTable`] view used outside
/// the tile system.
pub struct DataTablePanel {
    inner: Entity<DataTable>,
    label: SharedString,
}

impl DataTablePanel {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| new_data_table(db, cx));
        Self {
            inner,
            label: "Data Table".into(),
        }
    }

    pub fn from_config(_cfg: DataTablePanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::new(db, cx)
    }
}

impl Render for DataTablePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for DataTablePanel {
    type Config = DataTablePanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "data_table"
    }

    fn to_config(&self, _cx: &App) -> DataTablePanelConfig {
        DataTablePanelConfig {}
    }
}

/// Persisted shape of a [`BrowserPanel`]. No per-instance configuration today.
#[derive(facet::Facet, Default)]
pub struct BrowserPanelConfig {}

/// Pane item with a Finder-style browser over the component namespace tree.
pub struct BrowserPanel {
    inner: Entity<ComponentBrowser>,
    label: SharedString,
}

impl BrowserPanel {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| new_component_browser(db, cx));
        Self {
            inner,
            label: "Components".into(),
        }
    }

    pub fn from_config(_cfg: BrowserPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::new(db, cx)
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for BrowserPanel {
    type Config = BrowserPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "component_browser"
    }

    fn to_config(&self, _cx: &App) -> BrowserPanelConfig {
        BrowserPanelConfig {}
    }
}

/// Pane item hosting a time-series plot.
///
/// The panel is a thin wrapper around [`TimeSeriesPlot`]; inspection is
/// routed to the inner [`LinePlot`] so trace configuration shows up in the
/// property inspector.
pub struct PlotPanel {
    inner: Entity<TimeSeriesPlot>,
    line_plot: Entity<LinePlot>,
}

impl PlotPanel {
    /// Build a plot showing the selected elements of a single component.
    pub fn new(
        db: Arc<DB>,
        component_id: ComponentId,
        elements: &[usize],
        cx: &mut Context<Self>,
    ) -> Self {
        let inner = cx.new(|cx| TimeSeriesPlot::from_component(db, component_id, elements, cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self { inner, line_plot }
    }

    /// Build a plot with no traces; configure via the trace picker.
    pub fn empty(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::with_traces(db, vec![], cx)
    }

    /// Build a plot seeded with an explicit trace list.
    pub fn with_traces(db: Arc<DB>, traces: Vec<Trace>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| TimeSeriesPlot::new(db, traces, cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self { inner, line_plot }
    }

    pub(crate) fn inner(&self) -> &Entity<TimeSeriesPlot> {
        &self.inner
    }
}

impl Render for PlotPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

/// Persisted shape of a [`PlotPanel`].
///
/// `x_range` is intentionally absent: `TimeRangeBehavior::Offset` carries
/// `std::time::Duration`/`Timestamp` variants that aren't `Facet` yet, so it
/// can't round-trip through `facet-json` without a parallel config struct.
/// Adding it is a follow-up.
#[derive(facet::Facet, Default)]
pub struct PlotPanelConfig {
    pub label: String,
    pub traces: Vec<TraceConfig>,
    pub custom_title: Override<String>,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
}

/// Persisted shape of one [`Trace`].
///
/// `Trace` itself marks `component_id` and `element_index` as
/// `#[facet(skip)]` because the inspector doesn't expose them — but we *do*
/// need them on disk, so the persistence boundary uses this parallel
/// struct.
#[derive(facet::Facet, Clone)]
pub struct TraceConfig {
    pub component_id: ComponentId,
    pub element_index: usize,
    pub color: Hsla,
    pub style: PlotStyle,
    pub visible: bool,
    pub label: String,
    pub stroke_width: f32,
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
        }
    }
}

impl From<&Trace> for TraceConfig {
    fn from(t: &Trace) -> Self {
        Self {
            component_id: t.component_id,
            element_index: t.element_index,
            color: t.color,
            style: t.style,
            visible: t.visible,
            label: t.label.to_string(),
            stroke_width: t.stroke_width,
        }
    }
}

impl From<TraceConfig> for Trace {
    fn from(t: TraceConfig) -> Self {
        Self {
            component_id: t.component_id,
            element_index: t.element_index,
            color: t.color,
            style: t.style,
            visible: t.visible,
            label: t.label.into(),
            stroke_width: t.stroke_width,
        }
    }
}

impl PlotPanel {
    pub fn from_config(cfg: PlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let traces: Vec<Trace> = cfg.traces.into_iter().map(Trace::from).collect();
        let panel = Self::with_traces(db, traces, cx);
        let line_plot = panel.line_plot.clone();
        line_plot.update(cx, |lp, cx| {
            lp.custom_title = cfg.custom_title.map(SharedString::from);
            lp.y_min_override = cfg.y_min_override;
            lp.y_max_override = cfg.y_max_override;
            cx.notify();
        });
        panel
    }
}

impl PaneItem for PlotPanel {
    type Config = PlotPanelConfig;

    fn tab_title(&self, cx: &App) -> SharedString {
        self.inner.read(cx).title(cx)
    }

    fn serialization_key() -> &'static str {
        "time_series_plot"
    }

    fn to_config(&self, cx: &App) -> PlotPanelConfig {
        let lp = self.line_plot.read(cx);
        PlotPanelConfig {
            label: self.tab_title(cx).to_string(),
            traces: lp
                .traces()
                .iter()
                .map(|e| TraceConfig::from(e.read(cx)))
                .collect(),
            custom_title: lp.custom_title.as_ref().map(|s| s.to_string()),
            y_min_override: lp.y_min_override.clone(),
            y_max_override: lp.y_max_override.clone(),
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.line_plot.clone().into_any())
    }
}

/// Pane item hosting an XY (phase / correlation) plot.
///
/// Mirrors [`PlotPanel`]: thin wrapper around [`XyPlot`]; inspection
/// routes to the inner [`XyLinePlot`] so trace configuration shows up in
/// the property inspector.
pub struct XyPlotPanel {
    inner: Entity<XyPlot>,
    line_plot: Entity<XyLinePlot>,
}

impl XyPlotPanel {
    /// Build a plot with no traces; configure via the trace picker.
    pub fn empty(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::with_traces(db, vec![], cx)
    }

    /// Build a plot seeded with an explicit trace list.
    pub fn with_traces(db: Arc<DB>, traces: Vec<XyTrace>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| XyPlot::new(db, traces, cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self { inner, line_plot }
    }

    pub(crate) fn inner(&self) -> &Entity<XyPlot> {
        &self.inner
    }
}

impl Render for XyPlotPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

/// Persisted shape of an [`XyPlotPanel`].
#[derive(facet::Facet, Default)]
pub struct XyPlotPanelConfig {
    pub label: String,
    pub traces: Vec<XyTraceConfig>,
    pub custom_title: Override<String>,
    pub x_min_override: Override<f64>,
    pub x_max_override: Override<f64>,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
}

/// Persisted shape of one [`XyTrace`].
///
/// Mirrors [`TraceConfig`]; both axes' `(component_id, element_index)`
/// pairs need to round-trip on disk even though the inspector hides them.
#[derive(facet::Facet, Clone)]
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
    fn from(t: &XyTrace) -> Self {
        Self {
            x_component_id: t.x_component_id,
            x_element_index: t.x_element_index,
            y_component_id: t.y_component_id,
            y_element_index: t.y_element_index,
            color: t.color,
            style: t.style,
            visible: t.visible,
            label: t.label.to_string(),
            stroke_width: t.stroke_width,
        }
    }
}

impl From<XyTraceConfig> for XyTrace {
    fn from(t: XyTraceConfig) -> Self {
        Self {
            x_component_id: t.x_component_id,
            x_element_index: t.x_element_index,
            y_component_id: t.y_component_id,
            y_element_index: t.y_element_index,
            color: t.color,
            style: t.style,
            visible: t.visible,
            label: t.label.into(),
            stroke_width: t.stroke_width,
        }
    }
}

impl XyPlotPanel {
    pub fn from_config(cfg: XyPlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let traces: Vec<XyTrace> = cfg.traces.into_iter().map(XyTrace::from).collect();
        let panel = Self::with_traces(db, traces, cx);
        let line_plot = panel.line_plot.clone();
        line_plot.update(cx, |lp, cx| {
            lp.custom_title = cfg.custom_title.map(SharedString::from);
            lp.x_min_override = cfg.x_min_override;
            lp.x_max_override = cfg.x_max_override;
            lp.y_min_override = cfg.y_min_override;
            lp.y_max_override = cfg.y_max_override;
            cx.notify();
        });
        panel
    }
}

impl PaneItem for XyPlotPanel {
    type Config = XyPlotPanelConfig;

    fn tab_title(&self, cx: &App) -> SharedString {
        self.inner.read(cx).title(cx)
    }

    fn serialization_key() -> &'static str {
        "xy_plot"
    }

    fn to_config(&self, cx: &App) -> XyPlotPanelConfig {
        let lp = self.line_plot.read(cx);
        XyPlotPanelConfig {
            label: self.tab_title(cx).to_string(),
            traces: lp
                .traces()
                .iter()
                .map(|e| XyTraceConfig::from(e.read(cx)))
                .collect(),
            custom_title: lp.custom_title.as_ref().map(|s| s.to_string()),
            x_min_override: lp.x_min_override.clone(),
            x_max_override: lp.x_max_override.clone(),
            y_min_override: lp.y_min_override.clone(),
            y_max_override: lp.y_max_override.clone(),
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.line_plot.clone().into_any())
    }
}

/// Pane item hosting a list plot (index-vs-value of one component's
/// latest sample). Mirrors [`XyPlotPanel`] in shape; the inner
/// [`ListLinePlot`] owns trace state and is what the inspector edits.
pub struct ListPlotPanel {
    inner: Entity<ListPlot>,
    line_plot: Entity<ListLinePlot>,
}

impl ListPlotPanel {
    pub fn empty(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::with_traces(db, vec![], cx)
    }

    pub fn with_traces(db: Arc<DB>, traces: Vec<ListTrace>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| ListPlot::new(db, traces, cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self { inner, line_plot }
    }

    pub(crate) fn inner(&self) -> &Entity<ListPlot> {
        &self.inner
    }
}

impl Render for ListPlotPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

/// Persisted shape of a [`ListPlotPanel`].
#[derive(facet::Facet, Default)]
pub struct ListPlotPanelConfig {
    pub label: String,
    pub traces: Vec<ListTraceConfig>,
    pub custom_title: Override<String>,
    pub x_min_override: Override<f64>,
    pub x_max_override: Override<f64>,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
}

/// Persisted shape of one [`ListTrace`].
#[derive(facet::Facet, Clone)]
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
    fn from(t: &ListTrace) -> Self {
        Self {
            component_id: t.component_id,
            len: t.len,
            color: t.color,
            style: t.style,
            visible: t.visible,
            label: t.label.to_string(),
            stroke_width: t.stroke_width,
        }
    }
}

impl From<ListTraceConfig> for ListTrace {
    fn from(t: ListTraceConfig) -> Self {
        Self {
            component_id: t.component_id,
            len: t.len,
            color: t.color,
            style: t.style,
            visible: t.visible,
            label: t.label.into(),
            stroke_width: t.stroke_width,
        }
    }
}

impl ListPlotPanel {
    pub fn from_config(cfg: ListPlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let traces: Vec<ListTrace> = cfg.traces.into_iter().map(ListTrace::from).collect();
        let panel = Self::with_traces(db, traces, cx);
        let line_plot = panel.line_plot.clone();
        line_plot.update(cx, |lp, cx| {
            lp.custom_title = cfg.custom_title.map(SharedString::from);
            lp.x_min_override = cfg.x_min_override;
            lp.x_max_override = cfg.x_max_override;
            lp.y_min_override = cfg.y_min_override;
            lp.y_max_override = cfg.y_max_override;
            cx.notify();
        });
        panel
    }
}

impl PaneItem for ListPlotPanel {
    type Config = ListPlotPanelConfig;

    fn tab_title(&self, cx: &App) -> SharedString {
        self.inner.read(cx).title(cx)
    }

    fn serialization_key() -> &'static str {
        "list_plot"
    }

    fn to_config(&self, cx: &App) -> ListPlotPanelConfig {
        let lp = self.line_plot.read(cx);
        ListPlotPanelConfig {
            label: self.tab_title(cx).to_string(),
            traces: lp
                .traces()
                .iter()
                .map(|e| ListTraceConfig::from(e.read(cx)))
                .collect(),
            custom_title: lp.custom_title.as_ref().map(|s| s.to_string()),
            x_min_override: lp.x_min_override.clone(),
            x_max_override: lp.x_max_override.clone(),
            y_min_override: lp.y_min_override.clone(),
            y_max_override: lp.y_max_override.clone(),
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.line_plot.clone().into_any())
    }
}

/// Persisted shape of a [`Viewer3dPanel`].
#[derive(facet::Facet, Default)]
pub struct Viewer3dPanelConfig {
    pub models: Vec<ModelConfig>,
    pub camera: CameraConfig,
}

/// Persisted shape of one model entry inside a [`Viewer3dPanel`].
///
/// Mirrors the data fields of [`crate::views::viewer_3d::ModelEntry`] but
/// avoids the live `Entity` wrapping so the config can round-trip directly.
#[derive(facet::Facet, Default)]
pub struct ModelConfig {
    pub label: String,
    pub path: String,
    pub position_binding: Option<ComponentId>,
    pub orientation_binding: Option<ComponentId>,
}

/// Persisted shape of [`crate::views::viewer_3d::OrbitCamera`].
///
/// `glam::Vec3` is not `Facet`, so the target is unpacked into three fields
/// at the persistence boundary.
#[derive(facet::Facet)]
pub struct CameraConfig {
    pub target_x: f32,
    pub target_y: f32,
    pub target_z: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub fov_y_rad: f32,
}

impl Default for CameraConfig {
    /// Mirror [`crate::views::viewer_3d::OrbitCamera::default`] so a parse
    /// failure that falls back to `Default` still yields a usable camera.
    /// The auto-derived all-zero default would render nothing
    /// (`fov_y_rad == 0` collapses the projection matrix).
    fn default() -> Self {
        let cam = crate::views::viewer_3d::OrbitCamera::default();
        Self {
            target_x: cam.target.x,
            target_y: cam.target.y,
            target_z: cam.target.z,
            yaw: cam.yaw,
            pitch: cam.pitch,
            distance: cam.distance,
            fov_y_rad: cam.fov_y_rad,
        }
    }
}

/// Pane item hosting the Bevy-backed 3D viewer.
pub struct Viewer3dPanel {
    inner: Entity<Viewer3d>,
    label: SharedString,
}

impl Viewer3dPanel {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| Viewer3d::with_db(db, cx));
        Self {
            inner,
            label: "3D Viewer".into(),
        }
    }

    /// Rebuild a [`Viewer3dPanel`] from its persisted config.
    ///
    /// Spawns models through the public `add_model` API; bindings get reset
    /// directly on each [`crate::views::viewer_3d::ModelEntry`] entity so
    /// the viewer's reconcile pass picks them up on the next observe tick.
    pub fn from_config(cfg: Viewer3dPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| {
            let mut viewer = Viewer3d::with_db(db, cx);
            for model in &cfg.models {
                viewer.add_model(model.label.clone(), model.path.clone(), cx);
                if let Some(entry) = viewer.models().last().cloned() {
                    let pos = model.position_binding;
                    let orient = model.orientation_binding;
                    entry.update(cx, |m, cx| {
                        m.position_binding = pos;
                        m.orientation_binding = orient;
                        cx.notify();
                    });
                }
            }
            let cam = viewer.camera_mut();
            cam.target = glam::Vec3::new(
                cfg.camera.target_x,
                cfg.camera.target_y,
                cfg.camera.target_z,
            );
            cam.yaw = cfg.camera.yaw;
            cam.pitch = cfg.camera.pitch;
            cam.distance = cfg.camera.distance;
            cam.fov_y_rad = cfg.camera.fov_y_rad;
            viewer.camera_fov = cfg.camera.fov_y_rad;
            viewer.sync_camera(cx);
            viewer
        });
        Self {
            inner,
            label: "3D Viewer".into(),
        }
    }
}

impl Render for Viewer3dPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for Viewer3dPanel {
    type Config = Viewer3dPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "viewer_3d"
    }

    fn to_config(&self, cx: &App) -> Viewer3dPanelConfig {
        let inner = self.inner.read(cx);
        let cam = inner.camera();
        let models = inner
            .models()
            .iter()
            .map(|m| {
                let m = m.read(cx);
                ModelConfig {
                    label: m.label.to_string(),
                    path: m.path.clone(),
                    position_binding: m.position_binding_component(),
                    orientation_binding: m.orientation_binding_component(),
                }
            })
            .collect();
        Viewer3dPanelConfig {
            models,
            camera: CameraConfig {
                target_x: cam.target.x,
                target_y: cam.target.y,
                target_z: cam.target.z,
                yaw: cam.yaw,
                pitch: cam.pitch,
                distance: cam.distance,
                fov_y_rad: cam.fov_y_rad,
            },
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

/// Rows for the palette's "New Panel" submenu.
///
/// Each row adds a freshly-constructed panel to `pane`. The time-series row
/// detours through the trace picker, then calls `on_open_inspector` (if
/// provided) so the user can immediately configure the plot.
pub fn new_panel_rows(
    db: Arc<DB>,
    pane: Entity<Pane>,
    on_open_inspector: Option<OpenInspectorCallback>,
) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

    rows.push(Box::new(NavRow::new(
        "Time Series Plot",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            let on_open_inspector = on_open_inspector.clone();
            Box::new(move |_cx| {
                let db_for_select = db.clone();
                let pane = pane.clone();
                let on_open_inspector = on_open_inspector.clone();
                crate::inspector::trace_picker::select_traces_wizard_rows(
                    db.clone(),
                    Arc::new(|_cx| 0),
                    Arc::new(move |traces, window, cx| {
                        let db_for_panel = db_for_select.clone();
                        let plot_panel =
                            cx.new(|cx| PlotPanel::with_traces(db_for_panel, traces, cx));
                        let inner = plot_panel.read(cx).inner().clone();

                        pane.update(cx, |pane, cx| {
                            pane.add_item(Box::new(plot_panel), cx);
                        });

                        if let Some(on_open_inspector) = &on_open_inspector {
                            let inner_any = inner.into_any();
                            if let Some(rows) = crate::inspector::reflect::rows_for_any_entity(
                                &inner_any,
                                &db_for_select,
                                cx,
                            ) {
                                on_open_inspector(
                                    InspectorRequest {
                                        rows,
                                        mode: InspectorMode::Centered,
                                    },
                                    window,
                                    cx,
                                );
                            }
                        }
                    }),
                )
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "XY Plot",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            let on_open_inspector = on_open_inspector.clone();
            Box::new(move |_cx| {
                let db_for_select = db.clone();
                let pane = pane.clone();
                let on_open_inspector = on_open_inspector.clone();
                crate::views::xy_plot::trace_picker::select_xy_trace_wizard_rows(
                    db.clone(),
                    Arc::new(|_cx| 0),
                    Arc::new(move |trace, window, cx| {
                        let db_for_panel = db_for_select.clone();
                        let plot_panel =
                            cx.new(|cx| XyPlotPanel::with_traces(db_for_panel, vec![trace], cx));
                        let inner = plot_panel.read(cx).inner().clone();

                        pane.update(cx, |pane, cx| {
                            pane.add_item(Box::new(plot_panel), cx);
                        });

                        if let Some(on_open_inspector) = &on_open_inspector {
                            let inner_any = inner.into_any();
                            if let Some(rows) = crate::inspector::reflect::rows_for_any_entity(
                                &inner_any,
                                &db_for_select,
                                cx,
                            ) {
                                on_open_inspector(
                                    InspectorRequest {
                                        rows,
                                        mode: InspectorMode::Centered,
                                    },
                                    window,
                                    cx,
                                );
                            }
                        }
                    }),
                )
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "List Plot",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            let on_open_inspector = on_open_inspector.clone();
            Box::new(move |_cx| {
                let db_for_select = db.clone();
                let pane = pane.clone();
                let on_open_inspector = on_open_inspector.clone();
                crate::views::list_plot::trace_picker::select_list_trace_wizard_rows(
                    db.clone(),
                    Arc::new(|_cx| 0),
                    Arc::new(move |trace, window, cx| {
                        let db_for_panel = db_for_select.clone();
                        let plot_panel =
                            cx.new(|cx| ListPlotPanel::with_traces(db_for_panel, vec![trace], cx));
                        let inner = plot_panel.read(cx).inner().clone();

                        pane.update(cx, |pane, cx| {
                            pane.add_item(Box::new(plot_panel), cx);
                        });

                        if let Some(on_open_inspector) = &on_open_inspector {
                            let inner_any = inner.into_any();
                            if let Some(rows) = crate::inspector::reflect::rows_for_any_entity(
                                &inner_any,
                                &db_for_select,
                                cx,
                            ) {
                                on_open_inspector(
                                    InspectorRequest {
                                        rows,
                                        mode: InspectorMode::Centered,
                                    },
                                    window,
                                    cx,
                                );
                            }
                        }
                    }),
                )
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Component Text",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            Box::new(move |_cx| {
                let db_outer = db.clone();
                let pane = pane.clone();
                component_picker_rows(db.clone(), move |component_id, name, cx| {
                    let db = db_outer.clone();
                    pane.update(cx, |pane, cx| {
                        let item: Box<dyn PaneItemHandle> =
                            Box::new(cx.new(|cx| TextPanel::new(db, component_id, name, cx)));
                        pane.add_item(item, cx);
                    });
                })
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Traffic Light",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            Box::new(move |_cx| {
                let db_outer = db.clone();
                let pane = pane.clone();
                component_picker_rows(db.clone(), move |component_id, name, cx| {
                    let db = db_outer.clone();
                    pane.update(cx, |pane, cx| {
                        let item: Box<dyn PaneItemHandle> = Box::new(
                            cx.new(|cx| TrafficLightPanel::new(db, component_id, name, cx)),
                        );
                        pane.add_item(item, cx);
                    });
                })
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Traffic Light Grid",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            Box::new(move |_cx| traffic_light_grid_pattern_rows(db.clone(), pane.clone()))
        },
    )));

    rows.push(Box::new(CommandRow::new("Component Table", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> = Box::new(cx.new(|cx| TablePanel::new(db, cx)));
                pane.add_item(item, cx);
            });
        })
    })));

    rows.push(Box::new(CommandRow::new("Data Table", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> =
                    Box::new(cx.new(|cx| DataTablePanel::new(db, cx)));
                pane.add_item(item, cx);
            });
        })
    })));

    rows.push(Box::new(CommandRow::new("Component Browser", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> =
                    Box::new(cx.new(|cx| BrowserPanel::new(db, cx)));
                pane.add_item(item, cx);
            });
        })
    })));

    rows.push(Box::new(CommandRow::new("3D Viewer", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> =
                    Box::new(cx.new(|cx| Viewer3dPanel::new(db, cx)));
                pane.add_item(item, cx);
            });
        })
    })));

    rows.push(Box::new(CommandRow::new("Dashboard", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let dashboard = cx.new(|cx| DashboardPanel::new(db, cx));
                pane.add_item(Box::new(dashboard), cx);
            });
        })
    })));

    rows.push(Box::new(CommandRow::new("Node Editor", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let editor = cx.new(|cx| crate::node_editor::pane::NodeEditor::new(db.clone(), cx));
                pane.add_item(Box::new(editor), cx);
            });
        })
    })));

    rows
}

/// Single-question wizard for "New Panel → Traffic Light Grid": prompts
/// for a glob pattern, then constructs a [`TrafficLightGridPanel`] seeded
/// with that pattern.
fn traffic_light_grid_pattern_rows(db: Arc<DB>, pane: Entity<Pane>) -> Vec<Box<dyn InspectorRow>> {
    vec![crate::views::traffic_light_grid::glob_prompt_row(Arc::new(
        move |pattern, _window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> =
                    Box::new(cx.new(|cx| TrafficLightGridPanel::new(db, pattern, cx)));
                pane.add_item(item, cx);
            });
        },
    ))]
}

/// Rows listing every known component; selecting one invokes `on_select`.
pub fn component_picker_rows(
    db: Arc<DB>,
    on_select: impl Fn(ComponentId, String, &mut App) + 'static,
) -> Vec<Box<dyn InspectorRow>> {
    let on_select = Arc::new(on_select);
    crate::inspector::trace_picker::list_components(&db)
        .into_iter()
        .map(|(id, name)| {
            let on_select = on_select.clone();
            Box::new(CommandRow::new(
                SharedString::from(name.clone()),
                Arc::new(move |_window, cx| {
                    on_select(id, name.clone(), cx);
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_proto::types::ComponentId;

    /// Each panel's `*Config` round-trips through facet-json without loss.
    /// Mirrors the per-instance shape that `to_config` would produce; this
    /// test pins the wire format independently of the panel-construction
    /// code path so a missing field in either direction shows up here.
    #[test]
    fn panel_configs_round_trip_through_facet_json() {
        let text = TextPanelConfig {
            component: "altitude".into(),
        };
        let s = facet_json::to_string(&text).unwrap();
        let back: TextPanelConfig = facet_json::from_str(&s).unwrap();
        assert_eq!(back.component, "altitude");

        let plot = PlotPanelConfig {
            label: "speed".into(),
            traces: vec![TraceConfig {
                component_id: ComponentId(3),
                element_index: 1,
                color: Hsla::default(),
                style: PlotStyle::Line,
                visible: true,
                label: "vx".into(),
                stroke_width: 2.0,
            }],
            custom_title: Override::Custom("My View".into()),
            y_min_override: Override::Custom(-10.0),
            y_max_override: Override::Auto,
        };
        let s = facet_json::to_string(&plot).unwrap();
        let back: PlotPanelConfig = facet_json::from_str(&s).unwrap();
        assert_eq!(back.label, "speed");
        assert_eq!(back.traces.len(), 1);
        assert_eq!(back.traces[0].component_id, ComponentId(3));
        assert_eq!(back.traces[0].element_index, 1);
        assert_eq!(back.traces[0].label, "vx");
        assert!(matches!(back.custom_title, Override::Custom(s) if s == "My View"));
        assert!(matches!(back.y_min_override, Override::Custom(v) if (v + 10.0).abs() < 1e-9));
        assert!(matches!(back.y_max_override, Override::Auto));

        let viewer = Viewer3dPanelConfig {
            models: vec![ModelConfig {
                label: "satellite".into(),
                path: "sat.glb".into(),
                position_binding: Some(ComponentId(7)),
                orientation_binding: None,
            }],
            camera: CameraConfig {
                target_x: 1.0,
                target_y: 2.0,
                target_z: 3.0,
                yaw: 0.5,
                pitch: 0.25,
                distance: 10.0,
                fov_y_rad: std::f32::consts::FRAC_PI_3,
            },
        };
        let s = facet_json::to_string(&viewer).unwrap();
        let back: Viewer3dPanelConfig = facet_json::from_str(&s).unwrap();
        assert_eq!(back.models.len(), 1);
        assert_eq!(back.models[0].label, "satellite");
        assert_eq!(back.models[0].path, "sat.glb");
        assert_eq!(back.models[0].position_binding, Some(ComponentId(7)));
        assert_eq!(back.models[0].orientation_binding, None);
        assert_eq!(back.camera.target_x, 1.0);
        assert_eq!(back.camera.fov_y_rad, std::f32::consts::FRAC_PI_3);

        let xy = XyPlotPanelConfig {
            label: "phase".into(),
            traces: vec![XyTraceConfig {
                x_component_id: ComponentId(2),
                x_element_index: 0,
                y_component_id: ComponentId(2),
                y_element_index: 1,
                color: Hsla::default(),
                style: PlotStyle::Scatter,
                visible: true,
                label: "vx vs vy".into(),
                stroke_width: 1.5,
            }],
            custom_title: Override::Custom("Phase".into()),
            x_min_override: Override::Custom(-1.0),
            x_max_override: Override::Auto,
            y_min_override: Override::Auto,
            y_max_override: Override::Custom(2.5),
        };
        let s = facet_json::to_string(&xy).unwrap();
        let back: XyPlotPanelConfig = facet_json::from_str(&s).unwrap();
        assert_eq!(back.label, "phase");
        assert_eq!(back.traces.len(), 1);
        assert_eq!(back.traces[0].x_component_id, ComponentId(2));
        assert_eq!(back.traces[0].x_element_index, 0);
        assert_eq!(back.traces[0].y_component_id, ComponentId(2));
        assert_eq!(back.traces[0].y_element_index, 1);
        assert_eq!(back.traces[0].label, "vx vs vy");
        assert!(matches!(back.traces[0].style, PlotStyle::Scatter));
        assert!(matches!(back.custom_title, Override::Custom(s) if s == "Phase"));
        assert!(matches!(back.x_min_override, Override::Custom(v) if (v + 1.0).abs() < 1e-9));
        assert!(matches!(back.x_max_override, Override::Auto));
        assert!(matches!(back.y_min_override, Override::Auto));
        assert!(matches!(back.y_max_override, Override::Custom(v) if (v - 2.5).abs() < 1e-9));
    }
}
