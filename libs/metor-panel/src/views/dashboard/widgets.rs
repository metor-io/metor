//! Widget-kind registry and the leaf renderers that back built-in kinds.
//!
//! [`WidgetRegistry`] is a gpui `Global` mapping [`WidgetKind`] to its
//! [`WidgetSpec`]. Downstream consumers can register or override specs at
//! startup via [`WidgetRegistry::register`].
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyEntity, AnyView, App, Context, Entity, IntoElement, Render, RenderImage, SharedString,
    Window, div, prelude::*,
};
use metor_db::DB;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::theme::{theme, ui_px};
use crate::tiles::panels::{
    OutlinePanelConfig, alarm_config, alarm_view, browser_config, browser_view, log_config,
    log_view, outline_config, outline_view, sequence_config, sequence_view,
};
use crate::views::image_source::{self, ImageRef};
use crate::views::image_view::{ImageView, ImageViewConfig};
use crate::views::inspector_pane::InspectorPane;
use crate::views::viewer_3d::Viewer3d;
use crate::views::{
    Annunciator, AnnunciatorConfig, AttitudeConfig, AttitudeIndicator, ComponentBrowser,
    ComponentOutline, ExecTimeline, Gauge, GaugeConfig, ListPlot, Map, MapConfig, Meter,
    MeterConfig, Monitor, MonitorConfig, SamplesTable, SamplesTableConfig, SequenceControl,
    SequenceControlConfig, SequenceGrid, Spectrogram, StateChip, StateChipConfig, TimeSeriesPlot,
    Timeline, TrafficLight, TrafficLightConfig, XyPlot,
};
use crate::views::{Label, LabelConfig, StateImage, StateImageConfig};

use super::{DashboardWidget, WidgetKind};

/// Tile-host metadata for a registered view kind.
#[derive(Clone)]
pub struct TileViewSpec {
    pub serialization_key: SharedString,
    pub tab_title: Arc<dyn Fn(&AnyEntity, &str, &App) -> SharedString>,
}

/// Everything the dashboard needs to create and label one [`WidgetKind`].
///
/// Uses `Arc<dyn Fn>` so specs can capture owned state (shared resources,
/// remote handles) instead of being limited to plain function pointers.
pub struct WidgetSpec {
    pub default_size: (f32, f32),
    pub minimum_size: (f32, f32),
    pub label: Arc<dyn Fn(&DashboardWidget) -> SharedString>,
    /// Build receives the widget's persisted config string. Each builder
    /// chooses how to parse it (typically `serde_json::from_str` into a
    /// kind-specific config struct).
    pub build: Arc<dyn Fn(&str, &Arc<DB>, &mut App) -> WidgetLive>,
    /// Snapshot mutable state from the entity returned by `build`. Returning
    /// `None` keeps the last persisted blob verbatim.
    pub snapshot: Arc<dyn Fn(&AnyEntity, &str, &App) -> Option<String>>,
    pub tile: Option<TileViewSpec>,
}

/// The three runtime handles a host needs for one registered view.
///
/// `inspect` and `state` are deliberately separate: an outer view may paint
/// the UI while an inner model owns reflection metadata or persisted state.
#[derive(Clone)]
pub struct WidgetLive {
    pub view: AnyView,
    pub inspect: AnyEntity,
    pub state: AnyEntity,
}

impl WidgetSpec {
    /// Define a view kind in one place: construction, display naming, and
    /// live persistence. Register it through [`WidgetRegistry::register`]
    /// or [`crate::PanelApp::view`].
    pub fn new(
        default_size: (f32, f32),
        label: impl Fn(&DashboardWidget) -> SharedString + 'static,
        build: impl Fn(&str, &Arc<DB>, &mut App) -> WidgetLive + 'static,
        snapshot: impl Fn(&AnyEntity, &str, &App) -> Option<String> + 'static,
    ) -> Self {
        Self {
            default_size,
            minimum_size: (40.0, 40.0),
            label: Arc::new(label),
            build: Arc::new(build),
            snapshot: Arc::new(snapshot),
            tile: None,
        }
    }

    /// A view kind whose whole persisted state is one serde config: the blob
    /// parses (falling back to `Default`, so a stale file still opens) into
    /// `from_config`, and saving re-serializes `to_config` of the live view.
    pub fn typed<T, C>(
        default_size: (f32, f32),
        label: impl Fn(&DashboardWidget) -> SharedString + 'static,
        from_config: impl Fn(C, Arc<DB>, &mut Context<T>) -> T + 'static,
        to_config: impl Fn(&T, &App) -> C + 'static,
    ) -> Self
    where
        T: Render,
        C: Serialize + DeserializeOwned + Default,
    {
        Self::new(
            default_size,
            label,
            move |blob, db, cx| {
                let cfg = parse_or_default::<C>(blob);
                as_live(cx.new(|cx| from_config(cfg, db.clone(), cx)))
            },
            move |state, _, cx| {
                let view = state.clone().downcast::<T>().ok()?;
                serde_json::to_string(&to_config(view.read(cx), cx)).ok()
            },
        )
    }

    /// Set the smallest usable content size for dashboard resizing.
    pub fn with_minimum_size(mut self, size: (f32, f32)) -> Self {
        self.minimum_size = size;
        self
    }

    /// Hand the inspector an inner entity instead of the view itself. The
    /// plots paint through a wrapper but keep their traces on an inner model,
    /// and that model is what has the reflection adapters.
    pub fn inspecting<T: 'static>(mut self, inner: impl Fn(&T) -> AnyEntity + 'static) -> Self {
        let build = self.build.clone();
        self.build = Arc::new(move |blob, db, cx| {
            let mut live = build(blob, db, cx);
            if let Ok(state) = live.state.clone().downcast::<T>() {
                live.inspect = inner(state.read(cx));
            }
            live
        });
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
        });
        self
    }

    /// Derive the tab title from the live view when edits should be
    /// reflected immediately in the tab bar. The config-derived title from
    /// [`WidgetSpec::with_tile`] remains the fallback for a foreign state.
    pub fn with_live_tile_title<T: 'static>(
        mut self,
        tab_title: impl Fn(&T, &App) -> SharedString + 'static,
    ) -> Self {
        let tile = self
            .tile
            .as_mut()
            .expect("with_tile must be called before with_live_tile_title");
        let fallback = tile.tab_title.clone();
        tile.tab_title = Arc::new(
            move |state, config, cx| match state.clone().downcast::<T>() {
                Ok(view) => tab_title(view.read(cx), cx),
                Err(_) => fallback(state, config, cx),
            },
        );
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

    pub fn tile_specs(&self) -> Vec<Arc<WidgetSpec>> {
        self.specs
            .values()
            .filter(|spec| spec.tile.is_some())
            .cloned()
            .collect()
    }

    fn register_defaults(&mut self) {
        self.register(
            WidgetKind::plot(),
            WidgetSpec::typed(
                (400.0, 250.0),
                |w| numbered("Plot", w),
                TimeSeriesPlot::from_config,
                TimeSeriesPlot::to_config,
            )
            .inspecting(|plot: &TimeSeriesPlot| plot.line_plot().clone().into_any())
            .with_tile("time_series_plot", |_| "Plot".into())
            .with_live_tile_title(TimeSeriesPlot::title),
        );
        self.register(
            WidgetKind::new("xy_plot"),
            WidgetSpec::typed(
                (400.0, 300.0),
                |w| numbered("XY Plot", w),
                XyPlot::from_config,
                XyPlot::to_config,
            )
            .inspecting(|plot: &XyPlot| plot.line_plot().clone().into_any())
            .with_tile("xy_plot", |_| "XY Plot".into())
            .with_live_tile_title(XyPlot::title),
        );
        self.register(
            WidgetKind::new("list_plot"),
            WidgetSpec::typed(
                (400.0, 300.0),
                |w| numbered("List Plot", w),
                ListPlot::from_config,
                ListPlot::to_config,
            )
            .inspecting(|plot: &ListPlot| plot.line_plot().clone().into_any())
            .with_tile("list_plot", |_| "List Plot".into())
            .with_live_tile_title(ListPlot::title),
        );
        self.register(
            WidgetKind::new("spectrogram"),
            WidgetSpec::typed(
                (480.0, 300.0),
                |w| numbered("Spectrogram", w),
                Spectrogram::from_config,
                Spectrogram::to_config,
            )
            .inspecting(|plot: &Spectrogram| plot.plot().clone().into_any())
            .with_tile("spectrogram", |_| "Spectrogram".into())
            .with_live_tile_title(Spectrogram::title),
        );
        self.register(
            WidgetKind::new("timeline"),
            WidgetSpec::typed(
                (520.0, 112.0),
                |_| "Timeline".into(),
                Timeline::from_config,
                Timeline::to_config,
            )
            .with_minimum_size((80.0, 31.0))
            .with_tile("timeline", |_| "Timeline".into()),
        );
        self.register(
            WidgetKind::new("exec_timeline"),
            WidgetSpec::typed(
                (520.0, 260.0),
                |w| numbered("Execution Timeline", w),
                ExecTimeline::from_config,
                ExecTimeline::to_config,
            )
            .with_tile("exec_timeline", |_| "Execution".into())
            .with_live_tile_title(ExecTimeline::title),
        );
        // The outline replaced the component and data tables, so its
        // dashboard kind keeps the `table` id saved dashboards use.
        self.register(
            WidgetKind::table(),
            WidgetSpec::typed(
                (400.0, 300.0),
                |w| {
                    let cfg = parse_or_default::<OutlinePanelConfig>(&w.config);
                    if cfg.root.is_empty() {
                        numbered("Outline", w)
                    } else {
                        SharedString::from(format!("Outline: {}", cfg.root))
                    }
                },
                outline_view,
                outline_config,
            )
            .with_tile("component_outline", |_| "Outline".into())
            .with_live_tile_title(|outline: &ComponentOutline, cx| {
                // A rooted outline is named for its root's last segment, so a
                // pane of them reads `wheels | power | Outline`.
                let root = outline.root(cx);
                match root.rsplit('.').next() {
                    Some(last) if !last.is_empty() => SharedString::from(last.to_string()),
                    _ => SharedString::new_static("Outline"),
                }
            }),
        );
        self.register(
            WidgetKind::image(),
            WidgetSpec::new(
                (300.0, 200.0),
                |w| {
                    let cfg = parse_or_default::<ImageWidgetConfig>(&w.config);
                    subject("Image", &cfg.path)
                },
                |config, _db, cx| {
                    let cfg = parse_or_default::<ImageWidgetConfig>(config);
                    as_live(cx.new(|_cx| ImageWidget::load(&cfg)))
                },
                no_snapshot,
            ),
        );
        // Registered under both kinds: Component Text was folded into the
        // monitor, and saved dashboards keep naming those widgets `text`.
        let monitor = Arc::new(
            WidgetSpec::typed(
                (300.0, 160.0),
                |w| {
                    let cfg = parse_or_default::<MonitorConfig>(&w.config);
                    subject("Monitor", &cfg.component)
                },
                Monitor::from_config,
                |monitor, _| monitor.to_config(),
            )
            .with_tile("monitor", |blob| {
                parse_or_default::<MonitorConfig>(blob).component.into()
            }),
        );
        self.register_shared(WidgetKind::monitor(), monitor.clone());
        self.register_shared(WidgetKind::text(), monitor);
        self.register(
            WidgetKind::viewer3d(),
            WidgetSpec::typed(
                (480.0, 320.0),
                |w| numbered("3D Viewer", w),
                Viewer3d::from_config,
                Viewer3d::to_config,
            )
            .with_tile("viewer_3d", |_| "3D Viewer".into()),
        );
        self.register(
            WidgetKind::traffic_light(),
            WidgetSpec::typed(
                (120.0, 120.0),
                |w| {
                    let cfg = parse_or_default::<TrafficLightConfig>(&w.config);
                    subject("Traffic Light", &cfg.component)
                },
                TrafficLight::from_config,
                |light, _| light.to_config(),
            )
            .with_tile("traffic_light", |blob| {
                parse_or_default::<TrafficLightConfig>(blob)
                    .component
                    .into()
            }),
        );
        // Registered under both kinds: `traffic_light_grid` is the pre-rename
        // on-disk id, and a saved dashboard keeps naming it that.
        let annunciator = Arc::new(
            WidgetSpec::typed(
                (360.0, 200.0),
                |w| {
                    let cfg = parse_or_default::<AnnunciatorConfig>(&w.config);
                    subject("Annunciator", &cfg.pattern)
                },
                Annunciator::from_config,
                Annunciator::to_config,
            )
            .with_tile("annunciator", |_| "Annunciator".into()),
        );
        self.register_shared(WidgetKind::annunciator(), annunciator.clone());
        self.register_shared(WidgetKind::traffic_light_grid(), annunciator);
        self.register(
            WidgetKind::meter(),
            WidgetSpec::typed(
                (90.0, 200.0),
                |w| {
                    let cfg = parse_or_default::<MeterConfig>(&w.config);
                    subject("Meter", &cfg.label.unwrap_or(cfg.component))
                },
                |cfg, db, cx| Meter::from_config(&cfg, db, cx),
                |meter, _| meter.to_config(),
            )
            .with_tile("meter", |blob| {
                let cfg = parse_or_default::<MeterConfig>(blob);
                cfg.label.unwrap_or(cfg.component).into()
            }),
        );
        self.register(
            WidgetKind::gauge(),
            WidgetSpec::typed(
                (160.0, 140.0),
                |w| {
                    let cfg = parse_or_default::<GaugeConfig>(&w.config);
                    subject("Gauge", &cfg.label.unwrap_or(cfg.component))
                },
                |cfg, db, cx| Gauge::from_config(&cfg, db, cx),
                |gauge, _| gauge.to_config(),
            )
            .with_tile("gauge", |blob| {
                let cfg = parse_or_default::<GaugeConfig>(blob);
                cfg.label.unwrap_or(cfg.component).into()
            }),
        );
        self.register(
            WidgetKind::state_chip(),
            WidgetSpec::typed(
                (150.0, 60.0),
                |w| {
                    let cfg = parse_or_default::<StateChipConfig>(&w.config);
                    subject("State", &cfg.label.unwrap_or(cfg.component))
                },
                |cfg, db, cx| StateChip::from_config(&cfg, db, cx),
                StateChip::to_config,
            )
            .with_tile("state_chip", |blob| {
                let cfg = parse_or_default::<StateChipConfig>(blob);
                cfg.label.unwrap_or(cfg.component).into()
            }),
        );
        self.register(
            WidgetKind::sequence_control(),
            WidgetSpec::typed(
                (340.0, 48.0),
                |w| {
                    let cfg = parse_or_default::<SequenceControlConfig>(&w.config);
                    subject("Sequence", &cfg.channel)
                },
                |cfg, _db, cx| SequenceControl::from_config(&cfg, cx),
                |control, _| control.to_config(),
            )
            .with_tile("sequence_control", |blob| {
                parse_or_default::<SequenceControlConfig>(blob)
                    .channel
                    .into()
            }),
        );
        // No placeholder for an empty component: an unbound map is a usable
        // map, and the one the palette's zero-config row creates.
        self.register(
            WidgetKind::map(),
            WidgetSpec::typed(
                (400.0, 300.0),
                |w| {
                    let cfg = parse_or_default::<MapConfig>(&w.config);
                    subject("Map", &cfg.component)
                },
                |cfg, db, cx| Map::from_config(&cfg, db, cx),
                |map, _| map.to_config(),
            )
            .with_tile("map", |blob| {
                let cfg = parse_or_default::<MapConfig>(blob);
                if cfg.component.is_empty() {
                    "Map".into()
                } else {
                    cfg.component.into()
                }
            }),
        );
        self.register(
            WidgetKind::samples_table(),
            WidgetSpec::typed(
                (480.0, 320.0),
                |w| {
                    let cfg = parse_or_default::<SamplesTableConfig>(&w.config);
                    subject("Samples", &cfg.component)
                },
                |cfg, db, cx| SamplesTable::from_config(&cfg, db, cx),
                |table, _| table.to_config(),
            )
            .with_tile("samples_table", |_| "Samples".into())
            .with_live_tile_title(|table: &SamplesTable, _| table.component().clone()),
        );
        self.register(
            WidgetKind::image_view(),
            WidgetSpec::typed(
                (320.0, 320.0),
                |w| {
                    let cfg = parse_or_default::<ImageViewConfig>(&w.config);
                    subject("Image", &cfg.component)
                },
                |cfg, db, cx| ImageView::from_config(&cfg, db, cx),
                |view, _| view.to_config(),
            )
            .with_tile("image_view", |blob| {
                match parse_or_default::<ImageViewConfig>(blob).component {
                    name if name.is_empty() => "Image".into(),
                    name => name.into(),
                }
            })
            .with_live_tile_title(|view: &ImageView, _| view.title()),
        );
        self.register(
            WidgetKind::attitude(),
            WidgetSpec::typed(
                (220.0, 260.0),
                |w| {
                    let cfg = parse_or_default::<AttitudeConfig>(&w.config);
                    subject("Attitude", &cfg.label.unwrap_or(cfg.component))
                },
                |cfg, db, cx| AttitudeIndicator::from_config(&cfg, db, cx),
                AttitudeIndicator::to_config,
            )
            .with_tile("attitude", |blob| {
                let cfg = parse_or_default::<AttitudeConfig>(blob);
                cfg.label.unwrap_or(cfg.component).into()
            }),
        );
        self.register(
            WidgetKind::new("component_browser"),
            WidgetSpec::typed(
                (400.0, 300.0),
                |_| "Component Browser".into(),
                browser_view,
                browser_config,
            )
            .with_tile("component_browser", |_| "Components".into())
            .with_live_tile_title(|browser: &ComponentBrowser, _| browser.title()),
        );
        self.register(
            WidgetKind::new("alarm"),
            WidgetSpec::typed(
                (400.0, 300.0),
                |_| "Alarms".into(),
                alarm_view,
                alarm_config,
            )
            .with_tile("alarm", |_| "Alarms".into()),
        );
        self.register(
            WidgetKind::new("inspector"),
            WidgetSpec::typed(
                (320.0, 480.0),
                |_| "Inspector".into(),
                InspectorPane::from_config,
                InspectorPane::to_config,
            )
            .with_tile(crate::inspector::target::INSPECTOR_PANE_KEY, |_| {
                "Inspector".into()
            }),
        );
        self.register(
            WidgetKind::new("logs"),
            WidgetSpec::typed((480.0, 300.0), |_| "Logs".into(), log_view, log_config)
                .with_tile("logs", |_| "Logs".into()),
        );
        self.register(
            WidgetKind::new("sequence"),
            WidgetSpec::typed(
                (400.0, 300.0),
                |_| "Sequences".into(),
                sequence_view,
                sequence_config,
            )
            .with_tile("sequence", |_| "Sequences".into()),
        );
        self.register(
            WidgetKind::label(),
            WidgetSpec::typed(
                (120.0, 40.0),
                |w| {
                    let cfg = parse_or_default::<LabelConfig>(&w.config);
                    subject("Label", &cfg.text)
                },
                Label::from_config,
                Label::to_config,
            )
            .with_minimum_size((24.0, 20.0))
            .with_tile("label", |blob| {
                parse_or_default::<LabelConfig>(blob).text.into()
            })
            .with_live_tile_title(|label: &Label, _| label.text().clone()),
        );
        self.register(
            WidgetKind::state_image(),
            WidgetSpec::typed(
                (160.0, 120.0),
                |w| {
                    let cfg = parse_or_default::<StateImageConfig>(&w.config);
                    subject("State Image", &cfg.label.unwrap_or(cfg.component))
                },
                |cfg, db, cx| StateImage::from_config(&cfg, db, cx),
                StateImage::to_config,
            )
            .with_tile("state_image", |blob| {
                let cfg = parse_or_default::<StateImageConfig>(blob);
                cfg.label.unwrap_or(cfg.component).into()
            }),
        );
        // The grid reads the global sequence store and persists nothing.
        self.register(
            WidgetKind::new("sequence_grid"),
            WidgetSpec::new(
                (400.0, 300.0),
                |_| "Sequence Grid".into(),
                |_, _, cx| as_live(cx.new(SequenceGrid::new)),
                no_snapshot,
            )
            .with_tile("sequence_grid", |_| "Sequence Grid".into()),
        );
    }
}

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

/// Parse a widget's JSON blob into its expected config type, falling
/// back to `Default` on any parse error so labels and builders degrade
/// gracefully on a stale or hand-edited file.
fn parse_or_default<T: DeserializeOwned + Default>(blob: &str) -> T {
    serde_json::from_str::<T>(blob).unwrap_or_default()
}

/// `"{kind}: {subject}"`, with an unconfigured subject shown as `?` so the
/// label stays legible before the widget is bound.
fn subject(kind: &str, subject: &str) -> SharedString {
    let subject = if subject.is_empty() { "?" } else { subject };
    SharedString::from(format!("{kind}: {subject}"))
}

/// `"{kind} #{id}"`, for kinds with no single subject to name them by.
fn numbered(kind: &str, widget: &DashboardWidget) -> SharedString {
    SharedString::from(format!("{kind} #{}", widget.id.0))
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
    let label = SharedString::from(format!("? unknown kind: {}", kind.0));
    Arc::new(WidgetSpec::new(
        (200.0, 80.0),
        {
            let label = label.clone();
            move |_w| label.clone()
        },
        move |_config, _db, cx| {
            let label = label.clone();
            as_live(cx.new(|_cx| PlaceholderWidget { label }))
        },
        no_snapshot,
    ))
}

fn no_snapshot(_entity: &AnyEntity, _config: &str, _cx: &App) -> Option<String> {
    None
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

/// Widget that decodes an image and paints it into its bounds.
struct ImageWidget {
    render_image: Option<Arc<RenderImage>>,
    label: SharedString,
}

impl ImageWidget {
    fn load(cfg: &ImageWidgetConfig) -> Self {
        let loaded = image_source::load(&ImageRef {
            path: cfg.path.clone(),
            data: cfg.data.clone(),
        });
        match loaded {
            Ok(image) => Self {
                render_image: Some(image),
                label: SharedString::default(),
            },
            Err(label) => Self {
                render_image: None,
                label,
            },
        }
    }
}

impl Render for ImageWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(img) = &self.render_image {
            div()
                .size_full()
                .child(image_source::image_canvas(img.clone(), false))
        } else {
            let theme = theme(cx);
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_tertiary)
                .text_size(ui_px(14.0))
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

    /// Saved layouts name panes by these keys, so each must stay a tile of
    /// some built-in spec (the legacy table keys alias onto the outline's).
    #[test]
    fn every_built_in_pane_key_is_registered() {
        let mut reg = WidgetRegistry {
            specs: HashMap::new(),
        };
        reg.register_defaults();
        let keys: Vec<_> = reg
            .tile_specs()
            .iter()
            .filter_map(|spec| Some(spec.tile.as_ref()?.serialization_key.to_string()))
            .collect();
        for key in [
            "time_series_plot",
            "monitor",
            "xy_plot",
            "list_plot",
            "spectrogram",
            "timeline",
            "exec_timeline",
            "component_outline",
            "viewer_3d",
            "traffic_light",
            "annunciator",
            "meter",
            "gauge",
            "state_chip",
            "sequence_control",
            "map",
            "samples_table",
            "attitude",
            "component_browser",
            "alarm",
            "inspector",
            "logs",
            "sequence",
            "sequence_grid",
            "label",
            "state_image",
        ] {
            assert!(keys.iter().any(|k| k == key), "{key} has no tile");
        }
    }

    #[test]
    fn image_config_supports_local_path_only() {
        let cfg: ImageWidgetConfig = serde_json::from_str(r#"{"path":"/tmp/a.png"}"#).unwrap();
        assert_eq!(cfg.path, "/tmp/a.png");
        assert!(cfg.data.is_empty());
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
            .text_size(ui_px(14.0))
            .child(self.label.clone())
    }
}
