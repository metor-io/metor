//! Persisted shapes of the built-in panes, and the pane end of the shared
//! "add view" menu.
//!
//! Every built-in pane is a [`crate::views::dashboard::WidgetSpec`] with a
//! tile, so the configs here are what the registry parses for each key; the
//! `*_view` / `*_config` pairs are the spec's `from_config` / `to_config` for
//! the views that don't carry their own.
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{App, Context, Entity, SharedString};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use crate::inspector::OpenInspectorCallback;
use crate::inspector::rows::InspectorRow;
use crate::views::add_menu::{AddMenu, Host};
use crate::views::dashboard::WidgetRegistry;
use crate::views::outline::model::{PivotLayout, type_key};
use crate::views::time_series::Override;
use crate::views::{
    AlarmListMode, AlarmView, ComponentBrowser, ComponentOutline, LevelFilter, LogView,
    OutlineColumns, SequenceView, new_component_browser,
};
use crate::views::{Col, SequenceLayout, default_columns};

use super::pane::Pane;

/// Persisted shape of the alarm pane. It shows global alarm state, so the
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

/// Persisted shape of the log pane: the view's filters and follow mode.
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

/// Persisted shape of the sequence pane. Sequence state is global, so the
/// only persisted bit is the view's list mode (defaulted).
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SequencePanelConfig {
    pub show_history: bool,
    /// Absent in layouts saved before the list chose a control layout, which
    /// restore as [`SequenceLayout::Medium`], the list's own default.
    pub layout: Option<SequenceLayout>,
}

pub(crate) fn alarm_view(
    cfg: AlarmPanelConfig,
    _db: Arc<DB>,
    cx: &mut Context<AlarmView>,
) -> AlarmView {
    let mut view = AlarmView::new(cx);
    match cfg.mode {
        Some(mode) => view.mode = mode,
        None => view.set_history(cfg.show_history),
    }
    view
}

pub(crate) fn alarm_config(view: &AlarmView, _cx: &App) -> AlarmPanelConfig {
    AlarmPanelConfig {
        mode: Some(view.mode),
        show_history: view.is_history(),
    }
}

pub(crate) fn log_view(cfg: LogPanelConfig, _db: Arc<DB>, cx: &mut Context<LogView>) -> LogView {
    let mut view = LogView::new(cx);
    view.set_filters(cfg.min_level, cfg.source, cfg.follow);
    view
}

pub(crate) fn log_config(view: &LogView, _cx: &App) -> LogPanelConfig {
    LogPanelConfig {
        min_level: view.min_level,
        source: view.source.clone(),
        follow: view.follow,
    }
}

pub(crate) fn sequence_view(
    cfg: SequencePanelConfig,
    db: Arc<DB>,
    cx: &mut Context<SequenceView>,
) -> SequenceView {
    let mut view = SequenceView::new(db, cx);
    view.set_history(cfg.show_history);
    if let Some(layout) = cfg.layout {
        view.layout = layout;
    }
    view
}

pub(crate) fn sequence_config(view: &SequenceView, _cx: &App) -> SequencePanelConfig {
    SequencePanelConfig {
        show_history: view.is_history(),
        layout: Some(view.layout),
    }
}

/// Persisted shape of an outline: the filter bar, the columns in
/// display order, the sort, the root, which branches the user opened or
/// folded, and every pivot with its arrangement. Paths are full component
/// names; pivot fields are relative to an instance.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct OutlinePanelConfig {
    pub filter: String,
    pub filter_bar: bool,
    pub columns: Vec<String>,
    pub sort: SortDirection,
    pub root: String,
    pub expanded: Vec<String>,
    pub collapsed: Vec<String>,
    pub pivots: Vec<PivotConfig>,
    pub types: Vec<FrameTypeConfig>,
    pub focus: Option<String>,
}

impl Default for OutlinePanelConfig {
    fn default() -> Self {
        Self {
            filter: String::new(),
            filter_bar: false,
            columns: default_columns()
                .iter()
                .map(|c| c.key().to_string())
                .collect(),
            sort: SortDirection::Ascending,
            root: String::new(),
            expanded: Vec::new(),
            collapsed: Vec::new(),
            pivots: Vec::new(),
            types: Vec::new(),
            focus: None,
        }
    }
}

/// Sibling order in the outline's tree.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

/// A pivoted branch and how its grid is arranged: `fields` lead the
/// columns in that order, `hidden` ones are left out, `rows` lead the
/// instances by segment.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PivotConfig {
    pub path: String,
    pub fields: Vec<String>,
    pub hidden: Vec<String>,
    pub rows: Vec<String>,
}

/// A frame type the outline collected: a label, the leaf paths that
/// define its shape, and the same arrangement a [`PivotConfig`] carries
/// (`rows` being full instance paths).
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FrameTypeConfig {
    pub label: String,
    pub fields: Vec<String>,
    pub order: Vec<String>,
    pub hidden: Vec<String>,
    pub rows: Vec<String>,
}

impl PivotLayout {
    fn from_config(order: Vec<String>, hidden: Vec<String>, rows: Vec<String>) -> Self {
        Self {
            order: order.into_iter().map(SharedString::from).collect(),
            hidden: hidden.into_iter().map(SharedString::from).collect(),
            rows: rows.into_iter().map(SharedString::from).collect(),
        }
    }

    fn to_config(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let mut hidden: Vec<String> = self.hidden.iter().map(|s| s.to_string()).collect();
        hidden.sort();
        (
            self.order.iter().map(|s| s.to_string()).collect(),
            hidden,
            self.rows.iter().map(|s| s.to_string()).collect(),
        )
    }
}

/// An outline restored from `cfg`.
pub(crate) fn outline_view(
    cfg: OutlinePanelConfig,
    db: Arc<DB>,
    cx: &mut Context<ComponentOutline>,
) -> ComponentOutline {
    let mut outline = ComponentOutline::new(db, cx);
    outline.set_filter_visible(cfg.filter_bar, cx);
    outline.set_filter_text(&cfg.filter, cx);
    let columns: OutlineColumns = cfg.columns.iter().filter_map(|k| Col::parse(k)).collect();
    if !columns.is_empty() {
        outline.set_columns(columns, cx);
    }
    outline.set_descending(cfg.sort == SortDirection::Descending, cx);
    outline.set_root(&cfg.root, cx);
    outline.set_disclosure(cfg.expanded, cfg.collapsed, cx);
    let mut layouts = HashMap::new();
    let mut pivoted = Vec::with_capacity(cfg.pivots.len());
    for p in cfg.pivots {
        let layout = PivotLayout::from_config(p.fields, p.hidden, p.rows);
        if !layout.is_empty() {
            layouts.insert(SharedString::from(p.path.clone()), layout);
        }
        pivoted.push(p.path);
    }
    outline.set_pivoted_paths(pivoted, cx);
    let mut types = Vec::with_capacity(cfg.types.len());
    for t in cfg.types {
        let layout = PivotLayout::from_config(t.order, t.hidden, t.rows);
        if !layout.is_empty() {
            layouts.insert(type_key(&t.label), layout);
        }
        types.push((t.label, t.fields));
    }
    outline.set_types(types, cx);
    outline.set_pivot_layouts(layouts, cx);
    outline.set_focus(cfg.focus, cx);
    outline
}

/// The persisted shape of a live outline.
pub(crate) fn outline_config(inner: &ComponentOutline, cx: &App) -> OutlinePanelConfig {
    let layouts = inner.pivot_layouts(cx);
    let arrangement = |key: &SharedString| {
        layouts
            .get(key)
            .map(PivotLayout::to_config)
            .unwrap_or_default()
    };
    OutlinePanelConfig {
        filter: inner.filter_text(cx),
        filter_bar: inner.filter_visible(),
        columns: inner
            .columns(cx)
            .iter()
            .map(|c| c.key().to_string())
            .collect(),
        sort: if inner.descending(cx) {
            SortDirection::Descending
        } else {
            SortDirection::Ascending
        },
        root: inner.root(cx),
        expanded: inner.expanded_paths(cx),
        collapsed: inner.collapsed_paths(cx),
        pivots: inner
            .pivoted_paths(cx)
            .into_iter()
            .map(|path| {
                let (fields, hidden, rows) = arrangement(&SharedString::from(path.clone()));
                PivotConfig {
                    path,
                    fields,
                    hidden,
                    rows,
                }
            })
            .collect(),
        types: inner
            .types(cx)
            .into_iter()
            .map(|(label, fields)| {
                let (order, hidden, rows) = arrangement(&type_key(&label));
                FrameTypeConfig {
                    label,
                    fields,
                    order,
                    hidden,
                    rows,
                }
            })
            .collect(),
        focus: inner.focus(cx),
    }
}

/// Persisted shape of the component browser pane.
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

pub(crate) fn browser_view(
    cfg: BrowserPanelConfig,
    db: Arc<DB>,
    cx: &mut Context<ComponentBrowser>,
) -> ComponentBrowser {
    let mut browser = new_component_browser(db, cx);
    let custom = cfg.custom_title.map(SharedString::from);
    let segments: smallvec::SmallVec<[SharedString; 8]> = cfg
        .root_override
        .into_iter()
        .map(SharedString::from)
        .collect();
    let delegate = browser.delegate_mut();
    delegate.set_custom_title(custom, cx);
    if !segments.is_empty() {
        // Eager apply for the common case where the tree is already
        // populated; otherwise the watcher retries on each tree refresh
        // until `pending_root_path` resolves or the user overrides intent
        // (clear / new reroot).
        delegate.set_pending_root_path(Some(segments.clone()));
        delegate.set_root_path(&segments, cx);
    }
    browser.set_filter_visible(cfg.filter_bar, cx);
    browser.set_filter_text(&cfg.filter, cx);
    browser
}

pub(crate) fn browser_config(browser: &ComponentBrowser, cx: &App) -> BrowserPanelConfig {
    let delegate = browser.delegate();
    BrowserPanelConfig {
        custom_title: delegate.custom_title().clone().map(|s| s.to_string()),
        root_override: delegate
            .root_override()
            .map(|segs| segs.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default(),
        filter: browser.filter_text(cx),
        filter_bar: browser.filter_visible(),
    }
}

pub use crate::views::list_plot::{ListPlotPanelConfig, ListTraceConfig};
pub use crate::views::spectrogram::{SpectrogramPanelConfig, SpectrogramTraceConfig};
pub use crate::views::time_series::{
    EventOverlayConfig, MeasurementCursorConfig, MeasurementPanelConfig, PlotPanelConfig,
    TraceConfig, YAxisConfig,
};
pub use crate::views::viewer_3d::{CameraConfig, ModelConfig, Viewer3dPanelConfig};
pub use crate::views::xy_plot::{XyPlotPanelConfig, XyTraceConfig};

/// Construct a registered panel and return its inspectable entity.
pub(crate) fn add_registered_panel(
    pane: &Entity<Pane>,
    key: &str,
    config: &impl Serialize,
    cx: &mut App,
) -> Option<gpui::AnyEntity> {
    let state = serde_json::to_string(config).ok()?;
    add_registered_panel_state(pane, key, &state, cx)
}

/// Add a view by its registry key, returning the entity the inspector opens on.
pub(crate) fn add_registered_panel_state(
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

/// The "add view" menu aimed at `pane`. A widget kind opens under its tile
/// key; a kind with no widget spec (the dashboard itself) is a pane key
/// already.
pub(crate) fn new_panel_rows(
    db: Arc<DB>,
    pane: Entity<Pane>,
    on_open_inspector: Option<OpenInspectorCallback>,
) -> Vec<Box<dyn InspectorRow>> {
    pane_add_menu(db, pane, on_open_inspector).rows()
}

/// The [`AddMenu`] whose views land as new tabs in `pane`.
pub(crate) fn pane_add_menu(
    db: Arc<DB>,
    pane: Entity<Pane>,
    on_open_inspector: Option<OpenInspectorCallback>,
) -> AddMenu {
    AddMenu {
        db,
        host: Host::Pane,
        sink: Arc::new(move |kind, config, cx| {
            let key = cx
                .global::<WidgetRegistry>()
                .spec(kind)
                .and_then(|spec| Some(spec.tile.as_ref()?.serialization_key.clone()))
                .unwrap_or_else(|| kind.0.clone());
            add_registered_panel_state(&pane, &key, &config, cx)
        }),
        on_open_inspector,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::time_series::{PlotStyle, TimeFormat};
    use crate::views::{
        AnnunciatorConfig, AttitudeConfig, GaugeConfig, MeterConfig, MonitorConfig,
        SequenceControlConfig, StateChipConfig,
    };
    use gpui::Hsla;
    use metor_proto::types::ComponentId;

    /// Each panel's `*Config` round-trips through JSON without loss.
    /// Mirrors the per-instance shape that `to_config` would produce; this
    /// test pins the wire format independently of the panel-construction
    /// code path so a missing field in either direction shows up here.
    #[test]
    fn panel_configs_round_trip_through_json() {
        let monitor = MonitorConfig {
            component: "altitude".into(),
            ..Default::default()
        };
        let s = serde_json::to_string(&monitor).unwrap();
        let back: MonitorConfig = serde_json::from_str(&s).unwrap();
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
                position_expression: None,
                orientation_expression: None,
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
            shape: crate::views::MeterShape::Cylinder,
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
        assert_eq!(partial.shape, crate::views::MeterShape::Bar);

        let gauge = GaugeConfig {
            component: "sat.gyro".into(),
            element: 1,
            label: Some("rate y".into()),
            min: -0.2,
            max: 0.2,
            unit: Some("rad/s".into()),
            sweep_degrees: 200.0,
            rotation_degrees: -135.0,
            divisions: 4,
            minor_ticks: 0,
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
            style: crate::views::StateChipStyle::Toggle,
            text_size: 14.0,
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
            layout: SequenceLayout::Square,
        };
        let s = serde_json::to_string(&seq).unwrap();
        assert_eq!(
            serde_json::from_str::<SequenceControlConfig>(&s).unwrap(),
            seq
        );
        let partial: SequenceControlConfig = serde_json::from_str(r#"{"channel":"mode"}"#).unwrap();
        assert_eq!(partial.channel, "mode");
        assert_eq!(partial.layout, SequenceLayout::Medium);
        // Controls saved with the old `compact` flag, or the `compact` layout
        // that became medium, load as medium.
        for legacy in [
            r#"{"channel":"mode","compact":false}"#,
            r#"{"channel":"mode","layout":"compact"}"#,
        ] {
            let legacy: SequenceControlConfig = serde_json::from_str(legacy).unwrap();
            assert_eq!(legacy.layout, SequenceLayout::Medium);
        }

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

        let annunciator = AnnunciatorConfig {
            conditions: vec![],
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
        let back: AnnunciatorConfig = serde_json::from_str(&s).unwrap();
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
        let legacy: AnnunciatorConfig = serde_json::from_str(r#"{"pattern":"*.health"}"#).unwrap();
        assert_eq!(legacy.pattern, "*.health");
        assert_eq!(legacy.color, None);
        assert_eq!(legacy.source, crate::views::AnnunciatorSource::Components);
        assert_eq!(legacy.alarm_when, crate::views::AlarmWhen::On);
        assert!(!legacy.show_labels);
        assert!(!legacy.show_values);
        assert!(!legacy.latch);
        assert_eq!(legacy.columns, 0);
    }

    /// Cross-language pin: the outline the Python preset builder emits
    /// (`metor_config.Outline`) must parse into this config exactly.
    /// `test_golden.py` asserts the builder still produces the fixture;
    /// this asserts the panel still reads every field of it.
    #[test]
    fn the_golden_python_outline_parses() {
        let blob = include_str!("../../../metor-fsw-2/tests/golden/outline.json");
        let cfg: OutlinePanelConfig = serde_json::from_str(blob).unwrap();
        assert_eq!(cfg.root, "sat1.wheels");
        assert_eq!(cfg.columns, ["name", "value", "unit"]);
        assert_eq!(cfg.sort, SortDirection::Descending);
        assert_eq!(cfg.filter, "speed");
        assert!(cfg.filter_bar);
        assert_eq!(cfg.expanded, ["sat1.wheels.wheels.0"]);
        assert_eq!(cfg.collapsed, ["sat1.wheels.status"]);
        assert_eq!(cfg.pivots.len(), 1);
        assert_eq!(cfg.pivots[0].path, "sat1.wheels.wheels");
        assert_eq!(cfg.pivots[0].fields, ["speed", "torque"]);
        assert_eq!(cfg.pivots[0].hidden, ["motor.temp"]);
        assert_eq!(cfg.pivots[0].rows, ["3", "0"]);
        assert_eq!(cfg.types.len(), 1);
        assert_eq!(cfg.types[0].label, "psu");
        assert_eq!(cfg.types[0].fields, ["current", "voltage"]);
        assert_eq!(cfg.types[0].order, ["voltage"]);
        assert!(cfg.types[0].hidden.is_empty());
        assert_eq!(cfg.types[0].rows, ["sat1.dut2.psu"]);
        assert_eq!(cfg.focus.as_deref(), Some("psu"));
    }

    /// An empty pane state — what the legacy `ComponentTable()` and
    /// `DataTable()` builders emit — is the default outline.
    #[test]
    fn an_empty_outline_state_is_the_default() {
        let cfg: OutlinePanelConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.columns, ["name", "unit", "type", "value"]);
        assert_eq!(cfg.sort, SortDirection::Ascending);
        assert!(cfg.root.is_empty());
        assert!(cfg.pivots.is_empty());
    }
}
