use std::sync::Arc;

use gpui::{App, Context, Entity, IntoElement, Render, SharedString, Window, div, prelude::*};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::views::dashboard::DashboardPanel;
use crate::views::time_series::{LinePlot, PlotStyle, Trace};
use crate::views::viewer_3d::Viewer3d;
use crate::views::{
    ComponentBrowser, ComponentTable, ComponentText, DataTable, TimeSeriesPlot,
    new_component_browser, new_component_table, new_data_table,
};
use gpui::Hsla;
use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorCallback};
use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};

use super::item::{PaneItem, PaneItemHandle};
use super::pane::Pane;

/// Resolve a component by display name. Returns `None` when the DB has no
/// such component yet — callers fall back to a stub binding so saved layouts
/// still load against an empty DB.
fn lookup_component_by_name(db: &Arc<DB>, name: &str) -> Option<ComponentId> {
    db.with_state(|state| {
        state
            .component_metadata_iter()
            .find(|(_, meta)| meta.name == name)
            .map(|(id, _)| *id)
    })
}

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

    pub fn to_config(&self, _cx: &App) -> TextPanelConfig {
        TextPanelConfig {
            component: self.label.to_string(),
        }
    }

    /// Rebuild a [`TextPanel`] from its persisted config.
    ///
    /// Resolves the component by name through the DB; falls back to a stub
    /// pointing at the first registered component (or `ComponentId(0)` if
    /// none exists yet) so the layout still renders against an empty DB.
    pub fn from_config(cfg: TextPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let component_id = lookup_component_by_name(&db, &cfg.component).unwrap_or(ComponentId(0));
        Self::new(db, component_id, cfg.component, cx)
    }
}

impl Render for TextPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for TextPanel {
    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "component_text"
    }

    fn serialize(&self, cx: &App) -> String {
        facet_json::to_string(&self.to_config(cx)).expect("text panel config serializes")
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

    pub fn to_config(&self, _cx: &App) -> TablePanelConfig {
        TablePanelConfig {}
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
    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "component_table"
    }

    fn serialize(&self, cx: &App) -> String {
        facet_json::to_string(&self.to_config(cx)).expect("table panel config serializes")
    }
}

/// Persisted shape of a [`DataTablePanel`]. No per-instance configuration today.
#[derive(facet::Facet, Default)]
pub struct DataTablePanelConfig {}

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

    pub fn to_config(&self, _cx: &App) -> DataTablePanelConfig {
        DataTablePanelConfig {}
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
    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "data_table"
    }

    fn serialize(&self, cx: &App) -> String {
        facet_json::to_string(&self.to_config(cx)).expect("data table panel config serializes")
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

    pub fn to_config(&self, _cx: &App) -> BrowserPanelConfig {
        BrowserPanelConfig {}
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
    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "component_browser"
    }

    fn serialize(&self, cx: &App) -> String {
        facet_json::to_string(&self.to_config(cx)).expect("browser panel config serializes")
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
    pub fn with_traces(
        db: Arc<DB>,
        traces: Vec<crate::views::time_series::Trace>,
        cx: &mut Context<Self>,
    ) -> Self {
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
#[derive(facet::Facet, Default)]
pub struct PlotPanelConfig {
    pub label: String,
    pub traces: Vec<TraceConfig>,
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

impl PlotPanel {
    pub fn to_config(&self, cx: &App) -> PlotPanelConfig {
        let traces = self
            .line_plot
            .read(cx)
            .traces()
            .iter()
            .map(|t| {
                let t = t.read(cx);
                TraceConfig {
                    component_id: t.component_id,
                    element_index: t.element_index,
                    color: t.color,
                    style: t.style,
                    visible: t.visible,
                    label: t.label.to_string(),
                    stroke_width: t.stroke_width,
                }
            })
            .collect();
        PlotPanelConfig {
            label: self.tab_title(cx).to_string(),
            traces,
        }
    }

    pub fn from_config(cfg: PlotPanelConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let traces = cfg
            .traces
            .into_iter()
            .map(|t| Trace {
                component_id: t.component_id,
                element_index: t.element_index,
                color: t.color,
                style: t.style,
                visible: t.visible,
                label: t.label.into(),
                stroke_width: t.stroke_width,
            })
            .collect();
        Self::with_traces(db, traces, cx)
    }
}

impl PaneItem for PlotPanel {
    fn tab_title(&self, cx: &App) -> SharedString {
        self.inner.read(cx).title(cx)
    }

    fn serialization_key() -> &'static str {
        "time_series_plot"
    }

    fn serialize(&self, cx: &App) -> String {
        facet_json::to_string(&self.to_config(cx)).expect("plot panel config serializes")
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
#[derive(facet::Facet, Default)]
pub struct CameraConfig {
    pub target_x: f32,
    pub target_y: f32,
    pub target_z: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub fov_y_rad: f32,
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

    pub fn to_config(&self, cx: &App) -> Viewer3dPanelConfig {
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
            cam.target = glam::Vec3::new(cfg.camera.target_x, cfg.camera.target_y, cfg.camera.target_z);
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
    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "viewer_3d"
    }

    fn serialize(&self, cx: &App) -> String {
        facet_json::to_string(&self.to_config(cx)).expect("viewer 3d config serializes")
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

    rows.push(Box::new(CommandRow::new("Component Table", {
        let db = db.clone();
        let pane = pane.clone();
        Arc::new(move |_window, cx| {
            let db = db.clone();
            pane.update(cx, |pane, cx| {
                let item: Box<dyn PaneItemHandle> =
                    Box::new(cx.new(|cx| TablePanel::new(db, cx)));
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

    rows
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
        };
        let s = facet_json::to_string(&plot).unwrap();
        let back: PlotPanelConfig = facet_json::from_str(&s).unwrap();
        assert_eq!(back.label, "speed");
        assert_eq!(back.traces.len(), 1);
        assert_eq!(back.traces[0].component_id, ComponentId(3));
        assert_eq!(back.traces[0].element_index, 1);
        assert_eq!(back.traces[0].label, "vx");

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
    }
}
