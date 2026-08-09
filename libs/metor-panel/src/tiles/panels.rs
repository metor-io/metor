use std::sync::Arc;

use gpui::{App, Context, Entity, IntoElement, Render, SharedString, Window, div, prelude::*};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};
use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorCallback};
use crate::views::dashboard::DashboardPanel;
use crate::views::list_plot::{ListLinePlot, ListPlot, ListTrace};
use crate::views::time_series::{LinePlot, Override, Trace};
#[cfg(test)]
use crate::views::time_series::{PlotStyle, TimeFormat};
use crate::views::viewer_3d::Viewer3d;
use crate::views::xy_plot::{XyLinePlot, XyPlot, XyTrace};
use crate::views::{
    AlarmView, AttitudeConfig, AttitudeIndicator, ComponentBrowser, ComponentTable, ComponentText,
    DataTable, Gauge, GaugeConfig, LevelFilter, LogView, Meter, MeterConfig, SequenceControl,
    SequenceControlConfig, SequenceGrid, SequenceView, StateChip, StateChipConfig, TimeSeriesPlot,
    TrafficLight, TrafficLightGrid, new_component_browser, new_component_table, new_data_table,
};

use super::item::{PaneItem, PaneItemHandle};
use super::pane::Pane;

pub use crate::views::ComponentTextConfig as TextPanelConfig;

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

/// Persisted shape of an [`AlarmPanel`]. The panel shows global alarm state, so the
/// only persisted bit is which tab it opens on.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AlarmPanelConfig {
    pub show_history: bool,
}

/// Pane item listing the control system's alarms with acknowledge controls.
pub struct AlarmPanel {
    inner: Entity<AlarmView>,
}

impl AlarmPanel {
    pub fn new(_db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(AlarmView::new);
        Self { inner }
    }

    pub fn from_config(cfg: AlarmPanelConfig, _db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| {
            let mut view = AlarmView::new(cx);
            view.set_history(cfg.show_history);
            view
        });
        Self { inner }
    }
}

impl Render for AlarmPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for AlarmPanel {
    type Config = AlarmPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        SharedString::new_static("Alarms")
    }

    fn serialization_key() -> &'static str {
        "alarm"
    }

    fn to_config(&self, cx: &App) -> AlarmPanelConfig {
        AlarmPanelConfig {
            show_history: self.inner.read(cx).is_history(),
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into())
    }
}

/// Persisted shape of a [`LogPanel`]: the view's filters and follow mode.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct LogPanelConfig {
    pub min_level: LevelFilter,
    pub source: String,
    pub follow: bool,
}

impl Default for LogPanelConfig {
    fn default() -> Self {
        Self {
            min_level: LevelFilter::default(),
            source: String::new(),
            follow: true,
        }
    }
}

/// Pane item streaming the flight software's log lines.
pub struct LogPanel {
    inner: Entity<LogView>,
}

impl LogPanel {
    pub fn new(_db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(LogView::new);
        Self { inner }
    }

    pub fn from_config(cfg: LogPanelConfig, _db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| {
            let mut view = LogView::new(cx);
            view.set_filters(cfg.min_level, cfg.source, cfg.follow);
            view
        });
        Self { inner }
    }
}

impl Render for LogPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for LogPanel {
    type Config = LogPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        SharedString::new_static("Logs")
    }

    fn serialization_key() -> &'static str {
        "logs"
    }

    fn to_config(&self, cx: &App) -> LogPanelConfig {
        let view = self.inner.read(cx);
        LogPanelConfig {
            min_level: view.min_level,
            source: view.source.clone(),
            follow: view.follow,
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into())
    }
}

/// Persisted shape of a [`SequencePanel`]. Sequence state is global, so the only persisted
/// bit is the view's list mode (defaulted).
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SequencePanelConfig {
    pub show_history: bool,
}

/// Pane item with the detailed per-channel sequence control list.
pub struct SequencePanel {
    inner: Entity<SequenceView>,
}

impl SequencePanel {
    pub fn new(_db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(SequenceView::new);
        Self { inner }
    }

    pub fn from_config(cfg: SequencePanelConfig, _db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| {
            let mut view = SequenceView::new(cx);
            view.set_history(cfg.show_history);
            view
        });
        Self { inner }
    }
}

impl Render for SequencePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for SequencePanel {
    type Config = SequencePanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        SharedString::new_static("Sequences")
    }

    fn serialization_key() -> &'static str {
        "sequence"
    }

    fn to_config(&self, cx: &App) -> SequencePanelConfig {
        SequencePanelConfig {
            show_history: self.inner.read(cx).is_history(),
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into())
    }
}

/// Persisted shape of a [`SequenceGridPanel`]. No per-panel config — the grid reads the
/// global sequence store.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SequenceGridPanelConfig {}

/// Pane item with the compact many-channel sequence grid.
pub struct SequenceGridPanel {
    inner: Entity<SequenceGrid>,
}

impl SequenceGridPanel {
    pub fn new(_db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(SequenceGrid::new);
        Self { inner }
    }

    pub fn from_config(_cfg: SequenceGridPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::new(db, cx)
    }
}

impl Render for SequenceGridPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for SequenceGridPanel {
    type Config = SequenceGridPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        SharedString::new_static("Sequence Grid")
    }

    fn serialization_key() -> &'static str {
        "sequence_grid"
    }

    fn to_config(&self, _cx: &App) -> SequenceGridPanelConfig {
        SequenceGridPanelConfig::default()
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into())
    }
}

pub use crate::views::TrafficLightConfig as TrafficLightPanelConfig;

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

/// Pane item rendering one element of a component as a bar meter.
///
/// Carries no config of its own — [`MeterConfig`] is shared with the
/// dashboard widget, so a meter behaves identically on either surface.
pub struct MeterPanel {
    inner: Entity<Meter>,
    label: SharedString,
}

impl MeterPanel {
    pub fn from_config(cfg: MeterConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let label = SharedString::from(cfg.label.clone().unwrap_or_else(|| cfg.component.clone()));
        let inner = cx.new(|cx| Meter::from_config(&cfg, db, cx));
        Self { inner, label }
    }
}

impl Render for MeterPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for MeterPanel {
    type Config = MeterConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "meter"
    }

    fn to_config(&self, cx: &App) -> MeterConfig {
        self.inner.read(cx).to_config()
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

/// Pane item rendering one element of a component as a dial.
pub struct GaugePanel {
    inner: Entity<Gauge>,
    label: SharedString,
}

impl GaugePanel {
    pub fn from_config(cfg: GaugeConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let label = SharedString::from(cfg.label.clone().unwrap_or_else(|| cfg.component.clone()));
        let inner = cx.new(|cx| Gauge::from_config(&cfg, db, cx));
        Self { inner, label }
    }
}

impl Render for GaugePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for GaugePanel {
    type Config = GaugeConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "gauge"
    }

    fn to_config(&self, cx: &App) -> GaugeConfig {
        self.inner.read(cx).to_config()
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

/// Pane item rendering one element of a component as a named discrete state.
pub struct StateChipPanel {
    inner: Entity<StateChip>,
    label: SharedString,
}

impl StateChipPanel {
    pub fn from_config(cfg: StateChipConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let label = SharedString::from(cfg.label.clone().unwrap_or_else(|| cfg.component.clone()));
        let inner = cx.new(|cx| StateChip::from_config(&cfg, db, cx));
        Self { inner, label }
    }
}

impl Render for StateChipPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for StateChipPanel {
    type Config = StateChipConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "state_chip"
    }

    fn to_config(&self, cx: &App) -> StateChipConfig {
        self.inner.read(cx).to_config(cx)
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

/// Pane item rendering a quaternion component as an attitude ball.
pub struct AttitudePanel {
    inner: Entity<AttitudeIndicator>,
    label: SharedString,
}

impl AttitudePanel {
    pub fn from_config(cfg: AttitudeConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let label = SharedString::from(cfg.label.clone().unwrap_or_else(|| cfg.component.clone()));
        let inner = cx.new(|cx| AttitudeIndicator::from_config(&cfg, db, cx));
        Self { inner, label }
    }
}

impl Render for AttitudePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for AttitudePanel {
    type Config = AttitudeConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "attitude"
    }

    fn to_config(&self, cx: &App) -> AttitudeConfig {
        self.inner.read(cx).to_config(cx)
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

/// Pane item giving one sequence channel its own start/stop controls.
pub struct SequenceControlPanel {
    inner: Entity<SequenceControl>,
    label: SharedString,
}

impl SequenceControlPanel {
    pub fn from_config(cfg: SequenceControlConfig, _db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let label = SharedString::from(cfg.channel.clone());
        let inner = cx.new(|cx| SequenceControl::from_config(&cfg, cx));
        Self { inner, label }
    }
}

impl Render for SequenceControlPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for SequenceControlPanel {
    type Config = SequenceControlConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "sequence_control"
    }

    fn to_config(&self, cx: &App) -> SequenceControlConfig {
        self.inner.read(cx).to_config()
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

pub use crate::views::TrafficLightGridConfig as TrafficLightGridPanelConfig;

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
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
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
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
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

/// Persisted shape of a [`BrowserPanel`].
///
/// `root_override` is an empty `Vec` rather than `Option<Vec<_>>` because
/// "empty path" already encodes the no-override case and round-trips
/// through JSON without an extra discriminator. Filter-view state
/// (`SelectionRoot::Filter`) is not persisted yet.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrowserPanelConfig {
    pub custom_title: Override<String>,
    pub root_override: Vec<String>,
}

/// Pane item with a Finder-style browser over the component namespace tree.
pub struct BrowserPanel {
    inner: Entity<ComponentBrowser>,
}

impl BrowserPanel {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| new_component_browser(db, cx));
        Self { inner }
    }

    pub fn from_config(cfg: BrowserPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let panel = Self::new(db, cx);
        let custom = cfg.custom_title.map(SharedString::from);
        let segments: smallvec::SmallVec<[SharedString; 8]> = cfg
            .root_override
            .into_iter()
            .map(SharedString::from)
            .collect();
        panel.inner.update(cx, |browser, cx| {
            let delegate = browser.delegate_mut();
            delegate.set_custom_title(custom, cx);
            if !segments.is_empty() {
                // Eager apply for the common case where the tree is
                // already populated; otherwise the watcher retries on
                // each tree refresh until `pending_root_path` resolves
                // or the user overrides intent (clear / new reroot).
                delegate.set_pending_root_path(Some(segments.clone()));
                delegate.set_root_path(&segments, cx);
            }
        });
        panel
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for BrowserPanel {
    type Config = BrowserPanelConfig;

    fn tab_title(&self, cx: &App) -> SharedString {
        self.inner.read(cx).title()
    }

    fn serialization_key() -> &'static str {
        "component_browser"
    }

    fn to_config(&self, cx: &App) -> BrowserPanelConfig {
        let inner = self.inner.read(cx);
        let delegate = inner.delegate();
        BrowserPanelConfig {
            custom_title: delegate.custom_title().clone().map(|s| s.to_string()),
            root_override: delegate
                .root_override()
                .map(|segs| segs.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default(),
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
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
}

impl Render for PlotPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

pub use crate::views::time_series::{
    EventOverlayConfig, MeasurementCursorConfig, MeasurementPanelConfig, PlotPanelConfig,
    TraceConfig, YAxisConfig,
};

impl PlotPanel {
    pub fn from_config(cfg: PlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| TimeSeriesPlot::from_config(cfg, db, cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self { inner, line_plot }
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
        self.inner.read(cx).to_config(cx)
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
}

impl Render for XyPlotPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

pub use crate::views::xy_plot::{XyPlotPanelConfig, XyTraceConfig};

impl XyPlotPanel {
    pub fn from_config(cfg: XyPlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| XyPlot::from_config(cfg, db, cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self { inner, line_plot }
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
        self.inner.read(cx).to_config(cx)
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
}

impl Render for ListPlotPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

pub use crate::views::list_plot::{ListPlotPanelConfig, ListTraceConfig};

impl ListPlotPanel {
    pub fn from_config(cfg: ListPlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| ListPlot::from_config(cfg, db, cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self { inner, line_plot }
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
        self.inner.read(cx).to_config(cx)
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.line_plot.clone().into_any())
    }
}

pub use crate::views::viewer_3d::{CameraConfig, ModelConfig, Viewer3dPanelConfig};

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
        let inner = cx.new(|cx| Viewer3d::from_config(cfg, db, cx));
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
        self.inner.read(cx).to_config(cx)
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
fn add_registered_panel(
    pane: &Entity<Pane>,
    key: &str,
    config: &impl Serialize,
    cx: &mut App,
) -> Option<gpui::AnyEntity> {
    let state = serde_json::to_string(config).ok()?;
    add_registered_panel_state(pane, key, &state, cx)
}

fn add_registered_panel_state(
    pane: &Entity<Pane>,
    key: &str,
    state: &str,
    cx: &mut App,
) -> Option<gpui::AnyEntity> {
    let registry = cx.global::<super::ItemRegistry>().clone();
    pane.update(cx, |pane, cx| {
        let item = registry.deserialize(key, state, cx)?;
        let inspect = item.entity_any(cx);
        pane.add_item(item, cx);
        Some(inspect)
    })
}

fn inspect_created(
    entity: gpui::AnyEntity,
    db: &Arc<DB>,
    on_open: &Option<OpenInspectorCallback>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(on_open) = on_open else { return };
    let Some(rows) = crate::inspector::reflect::rows_for_any_entity(&entity, db, cx) else {
        return;
    };
    on_open(
        InspectorRequest {
            rows,
            mode: InspectorMode::Centered,
        },
        window,
        cx,
    );
}

pub(crate) fn new_panel_rows(
    db: Arc<DB>,
    pane: Entity<Pane>,
    on_open_inspector: Option<OpenInspectorCallback>,
    cx: &App,
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
                        let config = PlotPanelConfig {
                            traces: traces.iter().map(TraceConfig::from).collect(),
                            ..Default::default()
                        };
                        if let Some(entity) =
                            add_registered_panel(&pane, "time_series_plot", &config, cx)
                        {
                            inspect_created(entity, &db_for_select, &on_open_inspector, window, cx);
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
                        let config = XyPlotPanelConfig {
                            traces: vec![XyTraceConfig::from(&trace)],
                            ..Default::default()
                        };
                        if let Some(entity) = add_registered_panel(&pane, "xy_plot", &config, cx) {
                            inspect_created(entity, &db_for_select, &on_open_inspector, window, cx);
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
                        let config = ListPlotPanelConfig {
                            traces: vec![ListTraceConfig::from(&trace)],
                            ..Default::default()
                        };
                        if let Some(entity) = add_registered_panel(&pane, "list_plot", &config, cx)
                        {
                            inspect_created(entity, &db_for_select, &on_open_inspector, window, cx);
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
                let pane = pane.clone();
                crate::inspector::trace_picker::component_picker_rows(
                    db.clone(),
                    move |_component_id, name, cx| {
                        add_registered_panel(
                            &pane,
                            "component_text",
                            &TextPanelConfig { component: name },
                            cx,
                        );
                    },
                )
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
                let pane = pane.clone();
                crate::inspector::trace_picker::component_picker_rows(
                    db.clone(),
                    move |_component_id, name, cx| {
                        add_registered_panel(
                            &pane,
                            "traffic_light",
                            &TrafficLightPanelConfig {
                                component: name,
                                color: None,
                            },
                            cx,
                        );
                    },
                )
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Traffic Light Grid",
        SharedString::new_static(""),
        {
            let pane = pane.clone();
            Box::new(move |_cx| traffic_light_grid_pattern_rows(pane.clone()))
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Meter",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            Box::new(move |_cx| {
                instrument_wizard_rows(db.clone(), pane.clone(), "meter", |seed| {
                    serde_json::to_string(&MeterConfig::from(seed)).unwrap()
                })
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Gauge",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            Box::new(move |_cx| {
                instrument_wizard_rows(db.clone(), pane.clone(), "gauge", |seed| {
                    serde_json::to_string(&GaugeConfig::from(seed)).unwrap()
                })
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "State Chip",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            Box::new(move |_cx| {
                instrument_wizard_rows(db.clone(), pane.clone(), "state_chip", |seed| {
                    // A chip's state table can't be derived from the schema,
                    // so it opens showing the raw code until the operator
                    // names the states.
                    let cfg = StateChipConfig {
                        component: seed.component,
                        element: seed.element,
                        label: Some(seed.label),
                        ..Default::default()
                    };
                    serde_json::to_string(&cfg).unwrap()
                })
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Attitude",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            Box::new(move |_cx| {
                let pane = pane.clone();
                crate::inspector::trace_picker::component_picker_rows(
                    db.clone(),
                    move |_component_id, name, cx| {
                        let cfg = AttitudeConfig {
                            component: name,
                            ..Default::default()
                        };
                        add_registered_panel(&pane, "attitude", &cfg, cx);
                    },
                )
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Sequence Control",
        SharedString::new_static(""),
        {
            let pane = pane.clone();
            Box::new(move |cx| {
                let pane = pane.clone();
                crate::views::sequence_control::channel_picker_rows(cx, move |channel, cx| {
                    let cfg = SequenceControlConfig {
                        channel,
                        compact: false,
                    };
                    add_registered_panel(&pane, "sequence_control", &cfg, cx);
                })
            })
        },
    )));

    rows.push(Box::new(CommandRow::new("Component Table", {
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            add_registered_panel(&pane, "component_table", &TablePanelConfig {}, cx);
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
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            add_registered_panel(&pane, "viewer_3d", &Viewer3dPanelConfig::default(), cx);
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

    rows.push(Box::new(CommandRow::new("Alarms", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> = Box::new(cx.new(|cx| AlarmPanel::new(db, cx)));
                pane.add_item(item, cx);
            });
        })
    })));

    rows.push(Box::new(CommandRow::new("Logs", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> = Box::new(cx.new(|cx| LogPanel::new(db, cx)));
                pane.add_item(item, cx);
            });
        })
    })));

    rows.push(Box::new(CommandRow::new("Sequences", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> =
                    Box::new(cx.new(|cx| SequencePanel::new(db, cx)));
                pane.add_item(item, cx);
            });
        })
    })));

    rows.push(Box::new(CommandRow::new("Sequence Grid", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> =
                    Box::new(cx.new(|cx| SequenceGridPanel::new(db, cx)));
                pane.add_item(item, cx);
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

    rows.push(Box::new(CommandRow::new("System Graph", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> = Box::new(
                    cx.new(|cx| crate::views::system_graph::SystemGraphPanel::new(db, cx)),
                );
                pane.add_item(item, cx);
            });
        })
    })));

    if let Some(registry) = cx.try_global::<crate::views::dashboard::WidgetRegistry>() {
        for (label, add_flow) in registry.tile_add_flows() {
            rows.push(Box::new(NavRow::new(label, "", {
                let pane = pane.clone();
                let db = db.clone();
                Box::new(move |cx| add_flow(pane.clone(), db.clone(), cx))
            })));
        }
    }

    rows
}

/// Single-question wizard for "New Panel → Traffic Light Grid": prompts
/// for a glob pattern, then constructs a [`TrafficLightGridPanel`] seeded
/// with that pattern.
fn traffic_light_grid_pattern_rows(pane: Entity<Pane>) -> Vec<Box<dyn InspectorRow>> {
    vec![crate::views::traffic_light_grid::glob_prompt_row(Arc::new(
        move |pattern, _window, cx| {
            add_registered_panel(
                &pane,
                "traffic_light_grid",
                &TrafficLightGridPanelConfig {
                    pattern: pattern.to_string(),
                    color: None,
                },
                cx,
            );
        },
    ))]
}

use crate::views::instrument::{ScaleSeed, scale_seeds_for_traces};

/// The trace wizard wired to a scalar-instrument constructor: every picked
/// element becomes its own tile, built by `make`.
///
/// Meter, gauge, and chip differ only in what they construct from a
/// [`ScaleSeed`], so they share one wizard rather than three copies of the
/// same closure nest.
fn instrument_wizard_rows(
    db: Arc<DB>,
    pane: Entity<Pane>,
    key: &'static str,
    make_config: fn(ScaleSeed) -> String,
) -> Vec<Box<dyn InspectorRow>> {
    let db_outer = db.clone();
    crate::inspector::trace_picker::select_traces_wizard_rows(
        db,
        Arc::new(|_cx| 0),
        Arc::new(move |traces, _window, cx| {
            for seed in scale_seeds_for_traces(&db_outer, &traces, cx) {
                add_registered_panel_state(&pane, key, &make_config(seed), cx);
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Hsla;
    use metor_proto::types::ComponentId;

    /// Each panel's `*Config` round-trips through JSON without loss.
    /// Mirrors the per-instance shape that `to_config` would produce; this
    /// test pins the wire format independently of the panel-construction
    /// code path so a missing field in either direction shows up here.
    #[test]
    fn panel_configs_round_trip_through_json() {
        let text = TextPanelConfig {
            component: "altitude".into(),
        };
        let s = serde_json::to_string(&text).unwrap();
        let back: TextPanelConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.component, "altitude");

        let plot = PlotPanelConfig {
            label: "speed".into(),
            x_range: "LAST 30 min".into(),
            traces: vec![TraceConfig {
                component_id: ComponentId(3),
                element_index: 1,
                color: Hsla::default(),
                style: PlotStyle::Line,
                visible: true,
                label: "vx".into(),
                stroke_width: 2.0,
                axis_index: 1,
            }],
            custom_title: Override::Custom("My View".into()),
            axes: vec![
                YAxisConfig::default(),
                YAxisConfig {
                    label: "rpm".into(),
                    y_min_override: Override::Custom(0.0),
                    y_max_override: Override::Custom(8000.0),
                    color: Override::Auto,
                },
            ],
            x_time_format: TimeFormat::Utc,
            cursors: Vec::new(),
            measurement_panel: Default::default(),
            hide_alarm_limits: false,
            hide_alarm_color: false,
            event_overlays: vec![
                EventOverlayConfig {
                    kind: "alarms".into(),
                    label: "Alarms".into(),
                    visible: true,
                },
                EventOverlayConfig {
                    kind: "msg:e03c".into(),
                    label: "Widget".into(),
                    visible: false,
                },
            ],
        };
        let s = serde_json::to_string(&plot).unwrap();
        let back: PlotPanelConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.label, "speed");
        assert_eq!(back.event_overlays.len(), 2);
        assert_eq!(back.event_overlays[1].kind, "msg:e03c");
        assert!(!back.event_overlays[1].visible);
        // The stored kind parses back to the same key.
        assert_eq!(
            crate::plot_events::kind_key_from_string(&back.event_overlays[1].kind),
            Some(crate::plot_events::EventKindKey::Msg([0xe0, 0x3c]))
        );
        assert_eq!(back.traces[0].axis_index, 1);
        assert_eq!(back.axes.len(), 2);
        assert_eq!(back.axes[1].label, "rpm");
        assert!(
            matches!(back.axes[1].y_max_override, Override::Custom(v) if (v - 8000.0).abs() < 1e-6)
        );
        assert_eq!(back.traces.len(), 1);
        assert_eq!(back.traces[0].component_id, ComponentId(3));
        assert_eq!(back.traces[0].element_index, 1);
        assert_eq!(back.traces[0].label, "vx");
        assert!(matches!(back.custom_title, Override::Custom(s) if s == "My View"));
        assert_eq!(back.x_time_format, TimeFormat::Utc);
        assert_eq!(back.x_range, "LAST 30 min");
        assert!(
            back.x_range
                .parse::<crate::views::time_series::TimeRangeBehavior>()
                .is_ok()
        );

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
        let s = serde_json::to_string(&viewer).unwrap();
        let back: Viewer3dPanelConfig = serde_json::from_str(&s).unwrap();
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
        let s = serde_json::to_string(&xy).unwrap();
        let back: XyPlotPanelConfig = serde_json::from_str(&s).unwrap();
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

        let meter = MeterConfig {
            component: "sat.wheels.h".into(),
            element: 2,
            label: Some("wheel 2".into()),
            min: -0.04,
            max: 0.04,
            unit: Some("N·m·s".into()),
            orientation: crate::views::Orientation::Horizontal,
            color: Some(Hsla::default()),
            hide_value: true,
            hide_limits: false,
        };
        let s = serde_json::to_string(&meter).unwrap();
        let back: MeterConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back, meter);

        // A meter blob written before a field existed must still load, and
        // must not degrade to a zero-width scale.
        let partial: MeterConfig =
            serde_json::from_str(r#"{"component":"sat.wheels.h","element":1}"#).unwrap();
        assert_eq!(partial.component, "sat.wheels.h");
        assert_eq!(partial.element, 1);
        assert_eq!(partial.label, None);
        assert_eq!((partial.min, partial.max), (0.0, 1.0));
        assert!(matches!(
            partial.orientation,
            crate::views::Orientation::Vertical
        ));

        let gauge = GaugeConfig {
            component: "sat.gyro".into(),
            element: 1,
            label: Some("rate y".into()),
            min: -0.2,
            max: 0.2,
            unit: Some("rad/s".into()),
            sweep_degrees: 200.0,
            style: crate::views::GaugeStyle::Needle,
            color: Some(Hsla::default()),
            hide_value: false,
            hide_limits: true,
        };
        let s = serde_json::to_string(&gauge).unwrap();
        assert_eq!(serde_json::from_str::<GaugeConfig>(&s).unwrap(), gauge);

        // A gauge missing its sweep must not degenerate to a zero-width dial.
        let partial: GaugeConfig = serde_json::from_str(r#"{"component":"sat.gyro"}"#).unwrap();
        assert!(partial.sweep_degrees > 0.0);
        assert!(matches!(partial.style, crate::views::GaugeStyle::Arc));

        let chip = StateChipConfig {
            component: "sat.mode.mode_cmd".into(),
            element: 0,
            label: Some("mode".into()),
            states: vec![
                crate::views::StateEntryConfig {
                    value: 0.0,
                    label: "IDLE".into(),
                    color: None,
                },
                crate::views::StateEntryConfig {
                    value: 3.0,
                    label: "SAFE".into(),
                    color: Some(Hsla::default()),
                },
            ],
            unknown_label: "UNKNOWN".into(),
        };
        let s = serde_json::to_string(&chip).unwrap();
        let back: StateChipConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back, chip);
        assert_eq!(back.states[1].label, "SAFE");

        let partial: StateChipConfig =
            serde_json::from_str(r#"{"component":"sat.mode.mode_cmd"}"#).unwrap();
        assert!(partial.states.is_empty());
        assert!(partial.unknown_label.is_empty());

        let seq = SequenceControlConfig {
            channel: "mode".into(),
            compact: true,
        };
        let s = serde_json::to_string(&seq).unwrap();
        assert_eq!(
            serde_json::from_str::<SequenceControlConfig>(&s).unwrap(),
            seq
        );
        let partial: SequenceControlConfig = serde_json::from_str(r#"{"channel":"mode"}"#).unwrap();
        assert_eq!(partial.channel, "mode");
        assert!(!partial.compact);

        let attitude = AttitudeConfig {
            component: "sat.nav.attitude_estimate.q_hat_b_eci".into(),
            element_offset: 0,
            label: Some("estimate".into()),
            vectors: vec![crate::views::VectorMarkerConfig {
                component: "sat.plant.sensors.mag_b".into(),
                label: "mag".into(),
                color: Some(Hsla::default()),
            }],
            hide_readout: false,
        };
        let s = serde_json::to_string(&attitude).unwrap();
        let back: AttitudeConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back, attitude);
        assert_eq!(back.vectors[0].label, "mag");

        let partial: AttitudeConfig =
            serde_json::from_str(r#"{"component":"sat.body.q_b_eci"}"#).unwrap();
        assert_eq!(partial.element_offset, 0);
        assert!(partial.vectors.is_empty());
        assert!(!partial.hide_readout);
    }
}
