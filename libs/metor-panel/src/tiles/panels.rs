use std::sync::Arc;

use gpui::{App, Context, Entity, IntoElement, Render, SharedString, Window, div, prelude::*};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::views::dashboard::DashboardPanel;
use crate::views::time_series::LinePlot;
use crate::views::viewer_3d::Viewer3d;
use crate::views::{
    ComponentBrowser, ComponentTable, ComponentText, TimeSeriesPlot, new_component_browser,
    new_component_table,
};
use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorCallback};
use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};

use super::item::{PaneItem, PaneItemHandle};
use super::pane::Pane;

/// Tile panel wrapping a [`ComponentText`] display.
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

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({ "component": self.label.as_ref() })
    }
}

/// Tile panel wrapping a [`ComponentTable`].
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

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({})
    }
}

/// Tile panel wrapping a [`ComponentBrowser`] for Finder-style namespace navigation.
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

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({})
    }
}

/// Tile panel wrapping a [`TimeSeriesPlot`], with inspection support for trace configuration.
pub struct PlotPanel {
    inner: Entity<TimeSeriesPlot>,
    line_plot: Entity<LinePlot>,
}

impl PlotPanel {
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

    /// Create an empty plot panel, ready to be configured via the inspector.
    pub fn empty(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::with_traces(db, vec![], cx)
    }

    /// Create a plot panel pre-populated with the given traces.
    pub fn with_traces(
        db: Arc<DB>,
        traces: Vec<crate::views::time_series::Trace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let inner = cx.new(|cx| TimeSeriesPlot::new(db, traces, cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self { inner, line_plot }
    }

    /// The inner TimeSeriesPlot entity.
    pub(crate) fn inner(&self) -> &Entity<TimeSeriesPlot> {
        &self.inner
    }
}

impl Render for PlotPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for PlotPanel {
    fn tab_title(&self, cx: &App) -> SharedString {
        self.inner.read(cx).title(cx)
    }

    fn serialization_key() -> &'static str {
        "time_series_plot"
    }

    fn serialize(&self, cx: &App) -> serde_json::Value {
        let title = self.tab_title(cx);
        serde_json::json!({ "label": title.as_ref() })
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.line_plot.clone().into_any())
    }
}

/// Tile panel wrapping a [`Viewer3d`] with inspector support.
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

    fn serialize(&self, cx: &App) -> serde_json::Value {
        let inner = self.inner.read(cx);
        let cam = inner.camera();
        let models: Vec<serde_json::Value> = inner
            .models()
            .iter()
            .map(|m| {
                let m = m.read(cx);
                serde_json::json!({
                    "label": m.label.as_ref(),
                    "path": m.path,
                    "position_binding": m
                        .position_binding_component()
                        .map(|c| format!("{:?}", c)),
                    "orientation_binding": m
                        .orientation_binding_component()
                        .map(|c| format!("{:?}", c)),
                })
            })
            .collect();
        serde_json::json!({
            "models": models,
            "camera": {
                "target": [cam.target.x, cam.target.y, cam.target.z],
                "yaw": cam.yaw,
                "pitch": cam.pitch,
                "distance": cam.distance,
                "fov_y_rad": cam.fov_y_rad,
            },
        })
    }

    fn inspectable_entity(&self) -> Option<gpui::AnyEntity> {
        Some(self.inner.clone().into_any())
    }
}

/// Build the inspector rows for the "New Panel" submenu in the
/// everything-palette. Each row creates a new panel and adds it to `pane`.
///
/// Time-Series-Plot also auto-opens the trace wizard via `on_open_inspector`
/// (when supplied).
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

/// Build inspector rows listing available components, calling `on_select`
/// with the chosen component ID and name.
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
