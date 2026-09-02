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
    AlarmListMode, AlarmView, Annunciator, AttitudeConfig, AttitudeIndicator, ComponentBrowser,
    ComponentOutline, ComponentText, Gauge, GaugeConfig, LevelFilter, LogView, Meter, MeterConfig,
    OutlineColumns, SequenceControl, SequenceControlConfig, SequenceGrid, SequenceView, StateChip,
    StateChipConfig, TimeSeriesPlot, TrafficLight, new_component_browser,
};

use super::item::{PaneItem, PaneItemHandle};
use super::pane::Pane;

pub use crate::views::ComponentTextConfig as TextPanelConfig;

/// Pane item that renders a single component's latest value as text.
pub struct TextPanel {
    inner: Entity<ComponentText>,
    label: SharedString,
    _expression: Option<crate::dynamic::expressions::Expression>,
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
            _expression: None,
        }
    }

    pub fn from_config(cfg: TextPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let bound = crate::dynamic::expressions::bind(&cfg.component, &db, cx).ok();
        let component_id = bound
            .as_ref()
            .map(|bound| bound.id)
            .unwrap_or_else(|| ComponentId::new(&cfg.component));
        let mut panel = Self::new(db, component_id, cfg.component, cx);
        panel._expression = bound.and_then(|bound| bound.expression);
        panel
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
///
/// `show_history` is the pre-shelving spelling, kept as the fallback when `mode` is
/// absent so saved layouts restore unchanged.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AlarmPanelConfig {
    pub mode: Option<AlarmListMode>,
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
            match cfg.mode {
                Some(mode) => view.mode = mode,
                None => view.set_history(cfg.show_history),
            }
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
        let view = self.inner.read(cx);
        AlarmPanelConfig {
            mode: Some(view.mode),
            show_history: view.is_history(),
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
    _expression: Option<crate::dynamic::expressions::Expression>,
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
            _expression: None,
        }
    }

    pub fn from_config(cfg: TrafficLightPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let bound = crate::dynamic::expressions::bind(&cfg.component, &db, cx).ok();
        let component_id = bound
            .as_ref()
            .map(|bound| bound.id)
            .unwrap_or_else(|| ComponentId::new(&cfg.component));
        let inner = cx.new(|cx| TrafficLight::new(db, component_id, cx));
        if let Some(color) = cfg.color {
            inner.update(cx, |t, cx| t.set_color(color, cx));
        }
        Self {
            inner,
            label: cfg.component.into(),
            _expression: bound.and_then(|bound| bound.expression),
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

/// Pane item rendering a component's value as the named discrete state it
/// means — a bare value strip carrying a state table.
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

pub use crate::views::AnnunciatorConfig as AnnunciatorPanelConfig;

/// Pane item rendering every component matching a glob pattern as an
/// annunciator tile.
pub struct AnnunciatorPanel {
    inner: Entity<Annunciator>,
    label: SharedString,
}

impl AnnunciatorPanel {
    pub fn new(db: Arc<DB>, pattern: SharedString, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| Annunciator::new(db, pattern, cx));
        Self {
            inner,
            label: "Annunciator".into(),
        }
    }

    pub fn from_config(cfg: AnnunciatorPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| Annunciator::from_config(cfg, db, cx));
        Self {
            inner,
            label: "Annunciator".into(),
        }
    }
}

impl Render for AnnunciatorPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for AnnunciatorPanel {
    type Config = AnnunciatorPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "annunciator"
    }

    fn to_config(&self, cx: &App) -> AnnunciatorPanelConfig {
        self.inner.read(cx).to_config()
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

/// Persisted shape of an [`OutlinePanel`]: the filter bar, the sparkline
/// column, and which branches the user folded or opened.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct OutlinePanelConfig {
    pub filter: String,
    pub filter_bar: bool,
    pub sparklines: bool,
    #[serde(default = "shown")]
    pub unit: bool,
    #[serde(default = "shown")]
    pub type_column: bool,
    pub toggled: Vec<String>,
    pub pivoted: Vec<String>,
    pub types: Vec<FrameTypeConfig>,
    pub focus: Option<String>,
}

/// Columns are on unless a saved layout turned them off.
fn shown() -> bool {
    true
}

impl Default for OutlinePanelConfig {
    fn default() -> Self {
        Self {
            filter: String::new(),
            filter_bar: false,
            sparklines: false,
            unit: true,
            type_column: true,
            toggled: Vec::new(),
            pivoted: Vec::new(),
            types: Vec::new(),
            focus: None,
        }
    }
}

/// A frame type the outline collected: a label and the leaf paths that
/// define its shape.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FrameTypeConfig {
    pub label: String,
    pub fields: Vec<String>,
}

/// Pane item showing the component namespace as a collapsible tree-table.
pub struct OutlinePanel {
    inner: Entity<ComponentOutline>,
    label: SharedString,
}

impl OutlinePanel {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| ComponentOutline::new(db, cx));
        Self {
            inner,
            label: "Outline".into(),
        }
    }

    pub fn from_config(cfg: OutlinePanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let panel = Self::new(db, cx);
        panel.inner.update(cx, |outline, cx| {
            outline.set_filter_visible(cfg.filter_bar, cx);
            outline.set_filter_text(&cfg.filter, cx);
            outline.set_columns(
                OutlineColumns {
                    unit: cfg.unit,
                    ty: cfg.type_column,
                    sparkline: cfg.sparklines,
                },
                cx,
            );
            outline.set_toggled_paths(cfg.toggled, cx);
            outline.set_pivoted_paths(cfg.pivoted, cx);
            outline.set_types(
                cfg.types.into_iter().map(|t| (t.label, t.fields)).collect(),
                cx,
            );
            outline.set_focus(cfg.focus, cx);
        });
        panel
    }
}

impl Render for OutlinePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for OutlinePanel {
    type Config = OutlinePanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "component_outline"
    }

    fn to_config(&self, cx: &App) -> OutlinePanelConfig {
        let inner = self.inner.read(cx);
        OutlinePanelConfig {
            filter: inner.filter_text(cx),
            filter_bar: inner.filter_visible(),
            sparklines: inner.columns(cx).sparkline,
            unit: inner.columns(cx).unit,
            type_column: inner.columns(cx).ty,
            toggled: inner.toggled_paths(cx),
            pivoted: inner.pivoted_paths(cx),
            types: inner
                .types(cx)
                .into_iter()
                .map(|(label, fields)| FrameTypeConfig { label, fields })
                .collect(),
            focus: inner.focus(cx),
        }
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
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
    pub filter: String,
    pub filter_bar: bool,
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
            browser.set_filter_visible(cfg.filter_bar, cx);
            browser.set_filter_text(&cfg.filter, cx);
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
            filter: inner.filter_text(cx),
            filter_bar: inner.filter_visible(),
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
pub use crate::views::spectrogram::{SpectrogramPanelConfig, SpectrogramTraceConfig};

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
        "Spectrogram",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            let on_open_inspector = on_open_inspector.clone();
            Box::new(move |_cx| {
                let db_for_select = db.clone();
                let pane = pane.clone();
                let on_open_inspector = on_open_inspector.clone();
                crate::views::spectrogram::trace_picker::select_spectrogram_trace_wizard_rows(
                    db.clone(),
                    Arc::new(move |trace, window, cx| {
                        let config = SpectrogramPanelConfig {
                            traces: vec![SpectrogramTraceConfig::from(&trace)],
                            ..Default::default()
                        };
                        if let Some(entity) =
                            add_registered_panel(&pane, "spectrogram", &config, cx)
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
        "Annunciator",
        SharedString::new_static(""),
        {
            let pane = pane.clone();
            Box::new(move |_cx| annunciator_pattern_rows(pane.clone()))
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
        "Map",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            Box::new(move |_cx| {
                let pane = pane.clone();
                crate::inspector::trace_picker::component_picker_rows(
                    db.clone(),
                    move |_component_id, name, cx| {
                        let cfg = crate::views::MapConfig {
                            component: name,
                            ..Default::default()
                        };
                        add_registered_panel(&pane, "map", &cfg, cx);
                    },
                )
            })
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Samples",
        SharedString::new_static(""),
        {
            let db = db.clone();
            let pane = pane.clone();
            Box::new(move |_cx| {
                let pane = pane.clone();
                crate::inspector::trace_picker::component_picker_rows(
                    db.clone(),
                    move |_component_id, name, cx| {
                        let cfg = crate::views::SamplesTableConfig { component: name };
                        add_registered_panel(&pane, "samples_table", &cfg, cx);
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

    rows.push(Box::new(CommandRow::new("Outline", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> =
                    Box::new(cx.new(|cx| OutlinePanel::new(db, cx)));
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

    rows.push(Box::new(CommandRow::new("Execution Timeline", {
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            add_registered_panel(
                &pane,
                "exec_timeline",
                &crate::views::ExecTimelineConfig::default(),
                cx,
            );
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

/// Single-question wizard for "New Panel → Annunciator": prompts for a glob
/// pattern, then constructs an [`AnnunciatorPanel`] seeded with that pattern.
fn annunciator_pattern_rows(pane: Entity<Pane>) -> Vec<Box<dyn InspectorRow>> {
    vec![crate::views::annunciator::glob_prompt_row(Arc::new(
        move |pattern, _window, cx| {
            add_registered_panel(
                &pane,
                "annunciator",
                &crate::views::annunciator::seeded_config(&pattern),
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
                expression: Some("=rpm * 2.0".into()),
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

        let timeline = crate::views::ExecTimelineConfig {
            label: "Steps".into(),
            x_range: "LAST 30 s".into(),
            show_slots: false,
            show_coordinator_row: false,
            trigger: true,
            hidden_rows: vec!["downlink".into(), "nav".into()],
        };
        let s = serde_json::to_string(&timeline).unwrap();
        let back: crate::views::ExecTimelineConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.label, "Steps");
        assert_eq!(back.x_range, "LAST 30 s");
        assert!(!back.show_slots);
        assert!(!back.show_coordinator_row);
        assert!(back.trigger);
        assert_eq!(back.hidden_rows, vec!["downlink", "nav"]);
        // The toggles default on and the trigger off, so a document written
        // before they existed shows every lane on the app-wide range.
        let bare: crate::views::ExecTimelineConfig = serde_json::from_str("{}").unwrap();
        assert!(bare.show_slots && bare.show_coordinator_row && !bare.trigger);

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
                x_expression: None,
                y_expression: Some("=rpm * 2.0".into()),
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

        let spectrogram = SpectrogramPanelConfig {
            label: "spectrum".into(),
            traces: vec![SpectrogramTraceConfig {
                component_id: ComponentId(9),
                len: 33,
                visible: true,
                label: "fft(window(sig, 64))".into(),
                colormap: crate::views::time_series::Colormap::Mono,
                scale: crate::views::time_series::IntensityScale::Sqrt,
                gain: 2.5,
                expression: Some("=fft(window(sig, 64))".into()),
            }],
            custom_title: Override::Custom("Waterfall".into()),
            x_range: "LAST 30 s".into(),
            x_time_format: TimeFormat::Utc,
            y_min_override: Override::Custom(4.0),
            y_max_override: Override::Auto,
            intensity_min: Override::Custom(-90.0),
            intensity_max: Override::Custom(-10.0),
            sample_rate: Override::Custom(1000.0),
            show_colorbar: false,
        };
        let s = serde_json::to_string(&spectrogram).unwrap();
        let back: SpectrogramPanelConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.label, "spectrum");
        assert_eq!(back.traces.len(), 1);
        assert_eq!(back.traces[0].component_id, ComponentId(9));
        assert_eq!(back.traces[0].len, 33);
        assert_eq!(
            back.traces[0].colormap,
            crate::views::time_series::Colormap::Mono
        );
        assert_eq!(
            back.traces[0].scale,
            crate::views::time_series::IntensityScale::Sqrt
        );
        assert_eq!(back.traces[0].gain, 2.5);
        assert_eq!(
            back.traces[0].expression.as_deref(),
            Some("=fft(window(sig, 64))")
        );
        assert!(matches!(back.custom_title, Override::Custom(s) if s == "Waterfall"));
        assert_eq!(back.x_range, "LAST 30 s");
        assert_eq!(back.x_time_format, TimeFormat::Utc);
        assert!(matches!(back.y_min_override, Override::Custom(v) if (v - 4.0).abs() < 1e-9));
        assert!(matches!(back.intensity_max, Override::Custom(v) if (v + 10.0).abs() < 1e-9));
        assert!(matches!(back.sample_rate, Override::Custom(v) if (v - 1000.0).abs() < 1e-9));
        assert!(!back.show_colorbar);

        // A pane saved before the colorbar existed keeps the legend it had:
        // absent means "the default", and the default is on.
        let partial: SpectrogramPanelConfig = serde_json::from_str(r#"{"label":"x"}"#).unwrap();
        assert!(partial.show_colorbar);
        assert!(partial.traces.is_empty());

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

        let alarms = AlarmPanelConfig {
            mode: Some(AlarmListMode::Shelved),
            show_history: false,
        };
        let s = serde_json::to_string(&alarms).unwrap();
        let back: AlarmPanelConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.mode, Some(AlarmListMode::Shelved));

        // A layout saved before the mode was persisted restores through `show_history`.
        let legacy: AlarmPanelConfig = serde_json::from_str(r#"{"show_history":true}"#).unwrap();
        assert_eq!(legacy.mode, None);
        assert!(legacy.show_history);

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

        let annunciator = AnnunciatorPanelConfig {
            pattern: "*.healthy".into(),
            color: Some(Hsla::default()),
            source: crate::views::AnnunciatorSource::Alarms,
            alarm_when: crate::views::AlarmWhen::Off,
            show_labels: true,
            show_values: true,
            latch: true,
            columns: 4,
        };
        let s = serde_json::to_string(&annunciator).unwrap();
        let back: AnnunciatorPanelConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.pattern, "*.healthy");
        assert_eq!(back.color, Some(Hsla::default()));
        assert_eq!(back.source, crate::views::AnnunciatorSource::Alarms);
        assert_eq!(back.alarm_when, crate::views::AlarmWhen::Off);
        assert!(back.show_labels);
        assert!(back.show_values);
        assert!(back.latch);
        assert_eq!(back.columns, 4);
    }

    /// A grid saved before the annunciator rename carries only the two
    /// original fields; every field added since must default to what that
    /// layout used to render, `alarm_when: On` included so an old grid keeps
    /// lighting on truthy.
    #[test]
    fn a_pre_rename_grid_blob_keeps_its_old_behaviour() {
        let legacy: AnnunciatorPanelConfig =
            serde_json::from_str(r#"{"pattern":"*.health"}"#).unwrap();
        assert_eq!(legacy.pattern, "*.health");
        assert_eq!(legacy.color, None);
        assert_eq!(legacy.source, crate::views::AnnunciatorSource::Components);
        assert_eq!(legacy.alarm_when, crate::views::AlarmWhen::On);
        assert!(!legacy.show_labels);
        assert!(!legacy.show_values);
        assert!(!legacy.latch);
        assert_eq!(legacy.columns, 0);
    }
}
