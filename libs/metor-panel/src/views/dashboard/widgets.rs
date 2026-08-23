//! Widget-kind registry and the leaf renderers that back built-in kinds.
//!
//! [`WidgetRegistry`] is a gpui `Global` mapping [`WidgetKind`] to its
//! [`WidgetSpec`]. Downstream consumers can register or override specs at
//! startup via [`WidgetRegistry::register`].
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyView, App, Context, Corners, Entity, IntoElement, Render, RenderImage, SharedString, Window,
    canvas, div, prelude::*, px,
};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::DB;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::theme::theme;
use crate::views::time_series::{PlotPanelConfig, TimeSeriesPlot};
use crate::views::viewer_3d::{Viewer3d, Viewer3dPanelConfig};
use crate::views::{
    Annunciator, AttitudeConfig, AttitudeIndicator, ComponentText, Gauge, GaugeConfig, Meter,
    MeterConfig, Monitor, SequenceControl, SequenceControlConfig, StateChip, StateChipConfig,
    TrafficLight, new_component_table,
};
use crate::views::{ListPlot, ListPlotPanelConfig, XyPlot, XyPlotPanelConfig};

use super::{DashboardPanel, DashboardWidget, WidgetKind};

/// Arbitrary setup UI for adding a registered view to a dashboard.
pub type WidgetAddFlow = Arc<
    dyn Fn(
        Entity<DashboardPanel>,
        Arc<DB>,
        &App,
    ) -> Vec<Box<dyn crate::inspector::rows::InspectorRow>>,
>;

pub type TileAddFlow = Arc<
    dyn Fn(
        Entity<crate::tiles::Pane>,
        Arc<DB>,
        &App,
    ) -> Vec<Box<dyn crate::inspector::rows::InspectorRow>>,
>;

/// Tile-host metadata for a registered view kind.
#[derive(Clone)]
pub struct TileViewSpec {
    pub serialization_key: SharedString,
    pub tab_title: Arc<dyn Fn(&gpui::AnyEntity, &str, &App) -> SharedString>,
    pub add_flow: Option<(SharedString, TileAddFlow)>,
}

/// Everything the dashboard needs to create and label one [`WidgetKind`].
///
/// Uses `Arc<dyn Fn>` so specs can capture owned state (shared resources,
/// remote handles) instead of being limited to plain function pointers.
pub struct WidgetSpec {
    pub default_size: (f32, f32),
    pub label: Arc<dyn Fn(&DashboardWidget) -> SharedString>,
    /// Build receives the widget's persisted config string. Each builder
    /// chooses how to parse it (typically `serde_json::from_str` into a
    /// kind-specific config struct).
    pub build: Arc<dyn Fn(&str, &Arc<DB>, &mut App) -> WidgetLive>,
    /// Snapshot mutable state from the entity returned by `build`. Returning
    /// `None` keeps the last persisted blob verbatim.
    pub snapshot: Arc<dyn Fn(&gpui::AnyEntity, &str, &App) -> Option<String>>,
    pub add_flow: Option<(SharedString, WidgetAddFlow)>,
    pub tile: Option<TileViewSpec>,
}

/// The three runtime handles a host needs for one registered view.
///
/// `inspect` and `state` are deliberately separate: an outer view may paint
/// the UI while an inner model owns reflection metadata or persisted state.
#[derive(Clone)]
pub struct WidgetLive {
    pub view: AnyView,
    pub inspect: gpui::AnyEntity,
    pub state: gpui::AnyEntity,
}

impl WidgetSpec {
    /// Define a view kind in one place: construction, display naming, and
    /// live persistence. Register it through [`WidgetRegistry::register`]
    /// or [`crate::PanelApp::view`].
    pub fn new(
        default_size: (f32, f32),
        label: impl Fn(&DashboardWidget) -> SharedString + 'static,
        build: impl Fn(&str, &Arc<DB>, &mut App) -> WidgetLive + 'static,
        snapshot: impl Fn(&gpui::AnyEntity, &str, &App) -> Option<String> + 'static,
    ) -> Self {
        Self {
            default_size,
            label: Arc::new(label),
            build: Arc::new(build),
            snapshot: Arc::new(snapshot),
            add_flow: None,
            tile: None,
        }
    }

    /// Attach the picker or setup rows used by the Dashboard's “Add Widget”
    /// menu. The callback is deliberately open-ended so library views are not
    /// constrained to a closed set of setup workflows.
    pub fn with_add_flow(
        mut self,
        label: impl Into<SharedString>,
        add_flow: impl Fn(
            Entity<DashboardPanel>,
            Arc<DB>,
            &App,
        ) -> Vec<Box<dyn crate::inspector::rows::InspectorRow>>
        + 'static,
    ) -> Self {
        self.add_flow = Some((label.into(), Arc::new(add_flow)));
        self
    }

    /// Make the same registered view available as a regular tile pane.
    pub fn with_tile(
        mut self,
        serialization_key: impl Into<SharedString>,
        tab_title: impl Fn(&str) -> SharedString + 'static,
    ) -> Self {
        self.tile = Some(TileViewSpec {
            serialization_key: serialization_key.into(),
            tab_title: Arc::new(move |_state, config, _cx| tab_title(config)),
            add_flow: None,
        });
        self
    }

    /// Derive the tab title from the live state entity when edits should be
    /// reflected immediately in the tab bar.
    pub fn with_live_tile_title(
        mut self,
        tab_title: impl Fn(&gpui::AnyEntity, &str, &App) -> SharedString + 'static,
    ) -> Self {
        self.tile
            .as_mut()
            .expect("with_tile must be called before with_live_tile_title")
            .tab_title = Arc::new(tab_title);
        self
    }

    /// Attach arbitrary tile-side setup UI to this registered view.
    pub fn with_tile_add_flow(
        mut self,
        label: impl Into<SharedString>,
        add_flow: impl Fn(
            Entity<crate::tiles::Pane>,
            Arc<DB>,
            &App,
        ) -> Vec<Box<dyn crate::inspector::rows::InspectorRow>>
        + 'static,
    ) -> Self {
        let tile = self
            .tile
            .as_mut()
            .expect("with_tile must be called before with_tile_add_flow");
        tile.add_flow = Some((label.into(), Arc::new(add_flow)));
        self
    }
}

/// Global table of every widget kind the dashboard can render.
pub struct WidgetRegistry {
    specs: HashMap<WidgetKind, Arc<WidgetSpec>>,
}

impl gpui::Global for WidgetRegistry {}

impl WidgetRegistry {
    pub fn init(cx: &mut App) {
        let mut reg = Self {
            specs: HashMap::new(),
        };
        reg.register_defaults();
        cx.set_global(reg);
    }

    pub fn register(&mut self, kind: WidgetKind, spec: WidgetSpec) {
        self.register_shared(kind, Arc::new(spec));
    }

    pub fn register_shared(&mut self, kind: WidgetKind, spec: Arc<WidgetSpec>) {
        self.specs.insert(kind, spec);
    }

    pub fn spec(&self, kind: &WidgetKind) -> Option<Arc<WidgetSpec>> {
        self.specs.get(kind).cloned()
    }

    pub fn add_flows(&self) -> Vec<(SharedString, WidgetAddFlow)> {
        let mut flows: Vec<_> = self
            .specs
            .values()
            .filter_map(|spec| spec.add_flow.clone())
            .collect();
        flows.sort_by(|a, b| a.0.cmp(&b.0));
        flows
    }

    pub fn tile_specs(&self) -> Vec<Arc<WidgetSpec>> {
        self.specs
            .values()
            .filter(|spec| spec.tile.is_some())
            .cloned()
            .collect()
    }

    pub fn tile_add_flows(&self) -> Vec<(SharedString, TileAddFlow)> {
        let mut flows: Vec<_> = self
            .specs
            .values()
            .filter_map(|spec| spec.tile.as_ref()?.add_flow.clone())
            .collect();
        flows.sort_by(|a, b| a.0.cmp(&b.0));
        flows
    }

    fn register_defaults(&mut self) {
        self.register(
            WidgetKind::plot(),
            WidgetSpec::new(
                (400.0, 250.0),
                |w| SharedString::from(format!("Plot #{}", w.id.0)),
                build_plot,
                snapshot_plot,
            )
            .with_tile("time_series_plot", |_| "Plot".into())
            .with_live_tile_title(|state, _, cx| {
                state
                    .clone()
                    .downcast::<TimeSeriesPlot>()
                    .map(|plot| plot.read(cx).title(cx))
                    .unwrap_or_else(|_| "Plot".into())
            }),
        );
        self.register(
            WidgetKind::text(),
            WidgetSpec::new(
                (160.0, 60.0),
                |w| {
                    let cfg = parse_or_default::<TextWidgetConfig>(&w.config);
                    SharedString::from(format!("Text: {}", display_or_unknown(&cfg.component)))
                },
                build_text,
                no_snapshot,
            )
            .with_tile("component_text", |blob| {
                parse_or_default::<TextWidgetConfig>(blob).component.into()
            }),
        );
        self.register(
            WidgetKind::new("xy_plot"),
            WidgetSpec::new(
                (400.0, 300.0),
                |w| SharedString::from(format!("XY Plot #{}", w.id.0)),
                build_xy_plot,
                snapshot_xy_plot,
            )
            .with_tile("xy_plot", |_| "XY Plot".into())
            .with_live_tile_title(|state, _, cx| {
                state
                    .clone()
                    .downcast::<XyPlot>()
                    .map(|plot| plot.read(cx).title(cx))
                    .unwrap_or_else(|_| "XY Plot".into())
            }),
        );
        self.register(
            WidgetKind::new("list_plot"),
            WidgetSpec::new(
                (400.0, 300.0),
                |w| SharedString::from(format!("List Plot #{}", w.id.0)),
                build_list_plot,
                snapshot_list_plot,
            )
            .with_tile("list_plot", |_| "List Plot".into())
            .with_live_tile_title(|state, _, cx| {
                state
                    .clone()
                    .downcast::<ListPlot>()
                    .map(|plot| plot.read(cx).title(cx))
                    .unwrap_or_else(|_| "List Plot".into())
            }),
        );
        self.register(
            WidgetKind::table(),
            WidgetSpec::new(
                (400.0, 300.0),
                |w| SharedString::from(format!("Table #{}", w.id.0)),
                build_table,
                no_snapshot,
            )
            .with_tile("component_table", |_| "Components".into()),
        );
        self.register(
            WidgetKind::image(),
            WidgetSpec::new(
                (300.0, 200.0),
                |w| {
                    let cfg = parse_or_default::<ImageWidgetConfig>(&w.config);
                    SharedString::from(format!("Image: {}", display_or_unknown(&cfg.path)))
                },
                build_image,
                no_snapshot,
            ),
        );
        self.register(
            WidgetKind::monitor(),
            WidgetSpec::new(
                (300.0, 160.0),
                |w| {
                    let cfg = parse_or_default::<MonitorWidgetConfig>(&w.config);
                    SharedString::from(format!("Monitor: {}", display_or_unknown(&cfg.component)))
                },
                build_monitor,
                snapshot_monitor,
            ),
        );
        self.register(
            WidgetKind::viewer3d(),
            WidgetSpec::new(
                (480.0, 320.0),
                |w| SharedString::from(format!("3D Viewer #{}", w.id.0)),
                build_viewer3d,
                snapshot_viewer3d,
            )
            .with_tile("viewer_3d", |_| "3D Viewer".into()),
        );
        self.register(
            WidgetKind::traffic_light(),
            WidgetSpec::new(
                (120.0, 120.0),
                |w| {
                    let cfg = parse_or_default::<TrafficLightWidgetConfig>(&w.config);
                    SharedString::from(format!(
                        "Traffic Light: {}",
                        display_or_unknown(&cfg.component)
                    ))
                },
                build_traffic_light,
                snapshot_traffic_light,
            )
            .with_tile("traffic_light", |blob| {
                parse_or_default::<TrafficLightWidgetConfig>(blob)
                    .component
                    .into()
            }),
        );
        // Registered under both kinds: `traffic_light_grid` is the pre-rename
        // on-disk id, and a saved dashboard keeps naming it that.
        let annunciator = Arc::new(
            WidgetSpec::new(
                (360.0, 200.0),
                |w| {
                    let cfg = parse_or_default::<AnnunciatorWidgetConfig>(&w.config);
                    let label = if cfg.pattern.is_empty() {
                        "?".to_string()
                    } else {
                        cfg.pattern
                    };
                    SharedString::from(format!("Annunciator: {}", label))
                },
                build_annunciator,
                snapshot_annunciator,
            )
            .with_tile("annunciator", |_| "Annunciator".into()),
        );
        self.register_shared(WidgetKind::annunciator(), annunciator.clone());
        self.register_shared(WidgetKind::traffic_light_grid(), annunciator);
        self.register(
            WidgetKind::meter(),
            WidgetSpec::new(
                (90.0, 200.0),
                |w| {
                    let cfg = parse_or_default::<MeterConfig>(&w.config);
                    let name = cfg.label.unwrap_or(cfg.component);
                    SharedString::from(format!("Meter: {}", display_or_unknown(&name)))
                },
                build_meter,
                snapshot_typed::<Meter>,
            )
            .with_tile("meter", |blob| {
                let cfg = parse_or_default::<MeterConfig>(blob);
                cfg.label.unwrap_or(cfg.component).into()
            }),
        );
        self.register(
            WidgetKind::gauge(),
            WidgetSpec::new(
                (160.0, 140.0),
                |w| {
                    let cfg = parse_or_default::<GaugeConfig>(&w.config);
                    let name = cfg.label.unwrap_or(cfg.component);
                    SharedString::from(format!("Gauge: {}", display_or_unknown(&name)))
                },
                build_gauge,
                snapshot_typed::<Gauge>,
            )
            .with_tile("gauge", |blob| {
                let cfg = parse_or_default::<GaugeConfig>(blob);
                cfg.label.unwrap_or(cfg.component).into()
            }),
        );
        self.register(
            WidgetKind::state_chip(),
            WidgetSpec::new(
                (150.0, 60.0),
                |w| {
                    let cfg = parse_or_default::<StateChipConfig>(&w.config);
                    let name = cfg.label.unwrap_or(cfg.component);
                    SharedString::from(format!("State: {}", display_or_unknown(&name)))
                },
                build_state_chip,
                snapshot_state_chip,
            )
            .with_tile("state_chip", |blob| {
                let cfg = parse_or_default::<StateChipConfig>(blob);
                cfg.label.unwrap_or(cfg.component).into()
            }),
        );
        self.register(
            WidgetKind::sequence_control(),
            WidgetSpec::new(
                (260.0, 110.0),
                |w| {
                    let cfg = parse_or_default::<SequenceControlConfig>(&w.config);
                    SharedString::from(format!("Sequence: {}", display_or_unknown(&cfg.channel)))
                },
                build_sequence_control,
                snapshot_typed::<SequenceControl>,
            )
            .with_tile("sequence_control", |blob| {
                parse_or_default::<SequenceControlConfig>(blob)
                    .channel
                    .into()
            }),
        );
        self.register(
            WidgetKind::attitude(),
            WidgetSpec::new(
                (220.0, 260.0),
                |w| {
                    let cfg = parse_or_default::<AttitudeConfig>(&w.config);
                    let name = cfg.label.unwrap_or(cfg.component);
                    SharedString::from(format!("Attitude: {}", display_or_unknown(&name)))
                },
                build_attitude,
                snapshot_attitude,
            )
            .with_tile("attitude", |blob| {
                let cfg = parse_or_default::<AttitudeConfig>(blob);
                cfg.label.unwrap_or(cfg.component).into()
            }),
        );
    }
}

pub use crate::views::ComponentTextConfig as TextWidgetConfig;

/// Persisted shape of an image widget.
///
/// `data` carries the image itself, base64 (optionally as a `data:` URI), so
/// a target-shipped preset renders on a panel that cannot see the target's
/// filesystem. `path` is the local-authoring form.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImageWidgetConfig {
    pub path: String,
    pub data: String,
}

/// Persisted shape of a monitor widget — the source component to monitor
/// plus the inspector-editable display fields.
///
/// `unit` and `show_sparkline` are `Option`: `None` means "never edited", so restore keeps the
/// constructor's metadata-derived defaults instead of clobbering them.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MonitorWidgetConfig {
    pub component: String,
    pub unit: Option<String>,
    pub show_sparkline: Option<bool>,
}

pub use crate::views::AnnunciatorConfig as AnnunciatorWidgetConfig;
pub use crate::views::TrafficLightConfig as TrafficLightWidgetConfig;

/// Parse a widget's JSON blob into its expected config type, falling
/// back to `Default` on any parse error so labels and builders degrade
/// gracefully on a stale or hand-edited file.
fn parse_or_default<T: serde::de::DeserializeOwned + Default>(blob: &str) -> T {
    serde_json::from_str::<T>(blob).unwrap_or_default()
}

/// Render an empty string as `"?"` so labels stay legible when a widget
/// hasn't been configured yet.
fn display_or_unknown(s: &str) -> &str {
    if s.is_empty() { "?" } else { s }
}

/// Resolve `kind` to a spec. Unregistered kinds fall back to a placeholder
/// so a stale saved layout cannot crash the app.
pub(super) fn widget_spec(kind: &WidgetKind, cx: &App) -> Arc<WidgetSpec> {
    cx.global::<WidgetRegistry>()
        .spec(kind)
        .unwrap_or_else(|| placeholder_spec(kind))
}

/// Build a spec that draws a "? unknown kind" placeholder so removed or
/// renamed kinds degrade gracefully in older saved layouts.
fn placeholder_spec(kind: &WidgetKind) -> Arc<WidgetSpec> {
    let kind_name = kind.0.clone();
    Arc::new(WidgetSpec::new(
        (200.0, 80.0),
        {
            let kind_name = kind_name.clone();
            move |_w| SharedString::from(format!("? unknown kind: {}", kind_name))
        },
        move |_config, _db, cx| {
            let label = SharedString::from(format!("? unknown kind: {}", kind_name));
            as_live(cx.new(|_cx| PlaceholderWidget { label }))
        },
        no_snapshot,
    ))
}

fn no_snapshot(_entity: &gpui::AnyEntity, _config: &str, _cx: &App) -> Option<String> {
    None
}

fn snapshot_plot(entity: &gpui::AnyEntity, _config: &str, cx: &App) -> Option<String> {
    let plot = entity.clone().downcast::<TimeSeriesPlot>().ok()?;
    serde_json::to_string(&plot.read(cx).to_config(cx)).ok()
}

fn snapshot_traffic_light(entity: &gpui::AnyEntity, _config: &str, cx: &App) -> Option<String> {
    let tl = entity.clone().downcast::<TrafficLight>().ok()?;
    let v = tl.read(cx);
    serde_json::to_string(&TrafficLightWidgetConfig {
        component: v.name().to_string(),
        color: Some(v.color()),
    })
    .ok()
}

fn snapshot_annunciator(entity: &gpui::AnyEntity, _config: &str, cx: &App) -> Option<String> {
    let annunciator = entity.clone().downcast::<Annunciator>().ok()?;
    serde_json::to_string(&annunciator.read(cx).to_config()).ok()
}

fn snapshot_typed<T>(entity: &gpui::AnyEntity, _config: &str, cx: &App) -> Option<String>
where
    T: 'static,
    for<'a> &'a T: IntoSnapshot,
{
    let entity = entity.clone().downcast::<T>().ok()?;
    serde_json::to_string(&entity.read(cx).snapshot(cx)).ok()
}

trait IntoSnapshot {
    type Config: Serialize;
    fn snapshot(self, cx: &App) -> Self::Config;
}

impl IntoSnapshot for &Meter {
    type Config = MeterConfig;
    fn snapshot(self, _cx: &App) -> Self::Config {
        self.to_config()
    }
}

impl IntoSnapshot for &Gauge {
    type Config = GaugeConfig;
    fn snapshot(self, _cx: &App) -> Self::Config {
        self.to_config()
    }
}

impl IntoSnapshot for &SequenceControl {
    type Config = SequenceControlConfig;
    fn snapshot(self, _cx: &App) -> Self::Config {
        self.to_config()
    }
}

fn snapshot_state_chip(entity: &gpui::AnyEntity, _config: &str, cx: &App) -> Option<String> {
    let entity = entity.clone().downcast::<StateChip>().ok()?;
    serde_json::to_string(&entity.read(cx).to_config(cx)).ok()
}

fn snapshot_attitude(entity: &gpui::AnyEntity, _config: &str, cx: &App) -> Option<String> {
    let entity = entity.clone().downcast::<AttitudeIndicator>().ok()?;
    serde_json::to_string(&entity.read(cx).to_config(cx)).ok()
}

fn snapshot_monitor(entity: &gpui::AnyEntity, config: &str, cx: &App) -> Option<String> {
    let entity = entity.clone().downcast::<Monitor>().ok()?;
    let v = entity.read(cx);
    let mut cfg = parse_or_default::<MonitorWidgetConfig>(config);
    cfg.unit = Some(v.unit.to_string());
    cfg.show_sparkline = Some(v.show_sparkline);
    serde_json::to_string(&cfg).ok()
}

fn snapshot_viewer3d(entity: &gpui::AnyEntity, _config: &str, cx: &App) -> Option<String> {
    let entity = entity.clone().downcast::<Viewer3d>().ok()?;
    serde_json::to_string(&entity.read(cx).to_config(cx)).ok()
}

fn snapshot_xy_plot(entity: &gpui::AnyEntity, _config: &str, cx: &App) -> Option<String> {
    let entity = entity.clone().downcast::<XyPlot>().ok()?;
    serde_json::to_string(&entity.read(cx).to_config(cx)).ok()
}

fn snapshot_list_plot(entity: &gpui::AnyEntity, _config: &str, cx: &App) -> Option<String> {
    let entity = entity.clone().downcast::<ListPlot>().ok()?;
    serde_json::to_string(&entity.read(cx).to_config(cx)).ok()
}

/// Build the rendered view and its inspectable/state entities for a widget.
pub(super) fn create_widget_view(
    kind: &WidgetKind,
    config: &str,
    db: &Arc<DB>,
    cx: &mut App,
) -> WidgetLive {
    let spec = widget_spec(kind, cx);
    (spec.build)(config, db, cx)
}

fn as_live<T: Render + 'static>(e: Entity<T>) -> WidgetLive {
    let any = e.clone().into_any();
    WidgetLive {
        view: AnyView::from(e),
        inspect: any.clone(),
        state: any,
    }
}

fn build_plot(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    // Only LinePlot has Facet adapters, so expose it — not the outer
    // TimeSeriesPlot — as the inspectable entity.
    let cfg = parse_or_default::<PlotPanelConfig>(config);
    let plot = cx.new(|cx| TimeSeriesPlot::from_config(cfg, db.clone(), cx));
    let line_plot = plot.read(cx).line_plot().clone();
    WidgetLive {
        view: AnyView::from(plot.clone()),
        inspect: line_plot.into_any(),
        state: plot.into_any(),
    }
}

fn build_text(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<TextWidgetConfig>(config);
    if cfg.component.is_empty() {
        return as_live(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    let id = metor_proto::types::ComponentId::new(&cfg.component);
    as_live(cx.new(|cx| ComponentText::new(db.clone(), id, cx)))
}

fn build_xy_plot(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<XyPlotPanelConfig>(config);
    let plot = cx.new(|cx| XyPlot::from_config(cfg, db.clone(), cx));
    WidgetLive {
        view: AnyView::from(plot.clone()),
        inspect: plot.read(cx).line_plot().clone().into_any(),
        state: plot.into_any(),
    }
}

fn build_list_plot(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<ListPlotPanelConfig>(config);
    let plot = cx.new(|cx| ListPlot::from_config(cfg, db.clone(), cx));
    WidgetLive {
        view: AnyView::from(plot.clone()),
        inspect: plot.read(cx).line_plot().clone().into_any(),
        state: plot.into_any(),
    }
}

fn build_table(_config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    as_live(cx.new(|cx| new_component_table(db.clone(), cx)))
}

fn build_image(config: &str, _db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<ImageWidgetConfig>(config);
    as_live(cx.new(|_cx| ImageWidget::load(&cfg)))
}

fn build_monitor(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<MonitorWidgetConfig>(config);
    if cfg.component.is_empty() {
        return as_live(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    // A `=` binding is an expression rather than a component name: it is
    // compiled and started here, and the monitor holds a share of its
    // lifetime. What was serialized is the text the operator typed.
    let entity = if crate::dynamic::expressions::is_expression(&cfg.component) {
        let Ok(expression) = crate::dynamic::expressions::resolve(&cfg.component, db, cx) else {
            return as_live(cx.new(|_cx| PlaceholderWidget {
                label: SharedString::from(cfg.component.clone()),
            }));
        };
        let node = expression.node.clone();
        let entity = cx.new(|cx| Monitor::new(db.clone(), node, cx));
        entity.update(cx, |monitor, _cx| {
            monitor.bind_expression(expression, cfg.component.clone());
        });
        entity
    } else {
        let id = metor_proto::types::ComponentId::new(&cfg.component);
        cx.new(|cx| Monitor::new(db.clone(), id, cx))
    };
    if cfg.unit.is_some() || cfg.show_sparkline.is_some() {
        entity.update(cx, |m, cx| {
            if let Some(unit) = cfg.unit {
                m.unit = SharedString::from(unit);
            }
            if let Some(show) = cfg.show_sparkline {
                m.show_sparkline = show;
            }
            cx.notify();
        });
    }
    as_live(entity)
}

fn build_viewer3d(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<Viewer3dPanelConfig>(config);
    as_live(cx.new(|cx| Viewer3d::from_config(cfg, db.clone(), cx)))
}

fn build_traffic_light(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<TrafficLightWidgetConfig>(config);
    if cfg.component.is_empty() {
        return as_live(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    let id = metor_proto::types::ComponentId::new(&cfg.component);
    let entity = cx.new(|cx| TrafficLight::new(db.clone(), id, cx));
    if let Some(color) = cfg.color {
        entity.update(cx, |t, cx| t.set_color(color, cx));
    }
    as_live(entity)
}

fn build_meter(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<MeterConfig>(config);
    if cfg.component.is_empty() {
        return as_live(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    as_live(cx.new(|cx| Meter::from_config(&cfg, db.clone(), cx)))
}

fn build_gauge(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<GaugeConfig>(config);
    if cfg.component.is_empty() {
        return as_live(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    as_live(cx.new(|cx| Gauge::from_config(&cfg, db.clone(), cx)))
}

fn build_state_chip(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<StateChipConfig>(config);
    if cfg.component.is_empty() {
        return as_live(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    as_live(cx.new(|cx| StateChip::from_config(&cfg, db.clone(), cx)))
}

fn build_attitude(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<AttitudeConfig>(config);
    if cfg.component.is_empty() {
        return as_live(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    as_live(cx.new(|cx| AttitudeIndicator::from_config(&cfg, db.clone(), cx)))
}

fn build_sequence_control(config: &str, _db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<SequenceControlConfig>(config);
    as_live(cx.new(|cx| SequenceControl::from_config(&cfg, cx)))
}

fn build_annunciator(config: &str, db: &Arc<DB>, cx: &mut App) -> WidgetLive {
    let cfg = parse_or_default::<AnnunciatorWidgetConfig>(config);
    as_live(cx.new(|cx| Annunciator::from_config(cfg, db.clone(), cx)))
}

/// Widget that decodes an image and paints it into its bounds.
struct ImageWidget {
    render_image: Option<Arc<RenderImage>>,
    label: SharedString,
}

impl ImageWidget {
    /// Inline bytes win over `path`.
    ///
    /// A target ships its presets over a link and the panel may be running on
    /// a machine that has never seen the target's filesystem, so a diagram
    /// referenced only by path would be a broken tile there. `path` remains
    /// for layouts a user authored locally against their own files.
    fn load(cfg: &ImageWidgetConfig) -> Self {
        let (bytes, source) = match decode_inline(&cfg.data) {
            Some(bytes) => (Some(bytes), SharedString::new_static("inline image")),
            None => (
                std::fs::read(&cfg.path).ok(),
                SharedString::from(cfg.path.clone()),
            ),
        };

        let render_image = bytes
            .and_then(|bytes| image::load_from_memory(&bytes).ok())
            .map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, rgba.into_raw())
                    .expect("buffer size mismatch");
                let frames = SmallVec::from_elem(Frame::new(buffer), 1);
                Arc::new(RenderImage::new(frames))
            });

        let label = if render_image.is_some() {
            source
        } else {
            SharedString::from(format!("Failed to load: {}", source))
        };

        Self {
            render_image,
            label,
        }
    }
}

/// Decode an image widget's inline payload, accepting either raw base64 or a
/// `data:` URI so a preset author can paste either.
fn decode_inline(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    if data.is_empty() {
        return None;
    }
    let payload = match data.split_once("base64,") {
        Some((prefix, rest)) if prefix.starts_with("data:") => rest,
        _ => data,
    };
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()
}

impl Render for ImageWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(img) = &self.render_image {
            let img = img.clone();
            div().size_full().child(
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        let _ =
                            window.paint_image(bounds, Corners::default(), img.clone(), 0, false);
                    },
                )
                .size_full(),
            )
        } else {
            let theme = theme(cx);
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_tertiary)
                .text_size(px(14.0))
                .child(self.label.clone())
        }
    }
}

/// Grey "missing content" tile used whenever a widget can't instantiate.
struct PlaceholderWidget {
    label: SharedString,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_image_data_accepts_raw_base64_and_data_uris() {
        // "hi" in base64.
        assert_eq!(decode_inline("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(
            decode_inline("data:image/png;base64,aGk=").as_deref(),
            Some(&b"hi"[..])
        );
        // Whitespace from a wrapped literal in a preset file.
        assert_eq!(decode_inline(" aGk= \n").as_deref(), Some(&b"hi"[..]));
    }

    #[test]
    fn undecodable_inline_data_falls_through_to_the_path() {
        assert!(decode_inline("").is_none());
        assert!(decode_inline("not base64!!").is_none());
    }

    #[test]
    fn image_config_supports_local_path_only() {
        let cfg: ImageWidgetConfig = serde_json::from_str(r#"{"path":"/tmp/a.png"}"#).unwrap();
        assert_eq!(cfg.path, "/tmp/a.png");
        assert!(cfg.data.is_empty());
    }

    #[test]
    fn monitor_config_defaults_optional_display_fields() {
        let cfg: MonitorWidgetConfig = serde_json::from_str(r#"{"component":"a.b"}"#).unwrap();
        assert_eq!(cfg.component, "a.b");
        assert_eq!(cfg.unit, None);
        assert_eq!(cfg.show_sparkline, None);
    }

    #[test]
    fn monitor_config_round_trips_display_fields() {
        let cfg = MonitorWidgetConfig {
            component: "a.b".to_string(),
            unit: Some("V".to_string()),
            show_sparkline: Some(false),
        };
        let blob = serde_json::to_string(&cfg).unwrap();
        let back: MonitorWidgetConfig = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.component, "a.b");
        assert_eq!(back.unit.as_deref(), Some("V"));
        assert_eq!(back.show_sparkline, Some(false));
    }
}

#[allow(clippy::items_after_test_module)]
impl Render for PlaceholderWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.text_tertiary)
            .text_size(px(14.0))
            .child(self.label.clone())
    }
}
