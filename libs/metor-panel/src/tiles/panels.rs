use std::sync::Arc;

use gpui::{App, Context, Entity, IntoElement, Render, SharedString, Window, div, prelude::*};
use metor_db::DB;
use metor_proto::types::ComponentId;

use super::dashboard::DashboardPanel;
use crate::command_palette::{PaletteAction, PaletteItem, PalettePage};
use crate::elements::time_series::{LinePlot, OpenPageCallback};
use crate::elements::viewer_3d::Viewer3d;
use crate::elements::{ComponentTable, ComponentText, TimeSeriesPlot, new_component_table};

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

/// Tile panel wrapping a [`TimeSeriesPlot`], with inspection support for trace configuration.
pub struct PlotPanel {
    db: Arc<DB>,
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
        let inner =
            cx.new(|cx| TimeSeriesPlot::from_component(db.clone(), component_id, elements, cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self {
            db,
            inner,
            line_plot,
        }
    }

    /// Create an empty plot panel, ready to be configured via the inspector.
    pub fn empty(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| TimeSeriesPlot::new(db.clone(), vec![], cx));
        let line_plot = inner.read(cx).line_plot().clone();
        Self {
            db,
            inner,
            line_plot,
        }
    }

    /// The inner TimeSeriesPlot entity, for use with `palette_page_for_inspectable`.
    pub(crate) fn inner(&self) -> &Entity<TimeSeriesPlot> {
        &self.inner
    }

    /// The DB reference, needed for the inspectable palette.
    pub(crate) fn db(&self) -> &Arc<DB> {
        &self.db
    }

    /// Wire up the callback so right-clicking a legend item opens its inspector.
    pub fn set_on_open_page(&self, cb: OpenPageCallback, cx: &mut App) {
        self.inner.update(cx, |plot, _cx| {
            plot.set_on_open_page(cb);
        });
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
    db: Arc<DB>,
    label: SharedString,
}

impl Viewer3dPanel {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let db_clone = db.clone();
        let inner = cx.new(|cx| Viewer3d::with_db(db_clone, cx));
        Self {
            inner,
            db,
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

/// Callback invoked after a panel is created, so the caller can open its
/// inspectable palette if the panel has configurable fields.
pub type OnPanelCreated = Box<dyn FnOnce(Entity<PlotPanel>, &App) -> Option<PalettePage>>;

/// Build the root command palette page with "New Panel" and "Edit Panel" branches.
///
/// `on_inspect` is called when a panel needs configuration (newly created or
/// editing an existing one). The caller should display the returned PalettePage
/// in a CommandPalette.
pub fn tile_palette_page(
    db: Arc<DB>,
    pane: Entity<Pane>,
    tiles: &Entity<super::TileGroup>,
    on_inspect: impl Fn(PalettePage, &mut Window, &mut App) + 'static,
    on_open_inspector: Option<crate::inspector::OpenInspectorCallback>,
    cx: &App,
) -> PalettePage {
    let on_inspect = Arc::new(on_inspect);

    let mut items = vec![PaletteItem::new("New Panel", {
        let db = db.clone();
        let pane = pane.clone();
        let on_inspect = on_inspect.clone();
        let on_open_inspector = on_open_inspector.clone();
        PaletteAction::NextPage {
            label: Some("New".into()),
            page: Box::new(move || {
                new_panel_page(
                    db.clone(),
                    pane.clone(),
                    on_inspect.clone(),
                    on_open_inspector.clone(),
                )
            }),
        }
    })];

    // Only show "Edit Panel" if there are panels to edit
    let edit_labels = collect_edit_labels(tiles, cx);
    if !edit_labels.is_empty() {
        items.push(PaletteItem::new("Edit Panel", {
            PaletteAction::NextPage {
                label: Some("Edit".into()),
                page: Box::new(move || {
                    let edit_items: Vec<PaletteItem> = edit_labels
                        .iter()
                        .map(|label| {
                            PaletteItem::new(
                                label.clone(),
                                PaletteAction::NextPage {
                                    label: None,
                                    page: Box::new(|| PalettePage::new(vec![])),
                                },
                            )
                        })
                        .collect();
                    PalettePage::new(edit_items).prompt("Select panel to edit")
                }),
            }
        }));
    }

    items.push(PaletteItem::new(
        "Update Component",
        PaletteAction::NextPage {
            label: Some("Update".into()),
            page: Box::new({
                let db = db.clone();
                move || crate::pending_edits::update_component_page(db.clone())
            }),
        },
    ));

    let pending_count = crate::pending_edits::pending_edits(cx).edits.len();
    if pending_count > 0 {
        let label = SharedString::from(format!("Review Edits ({})", pending_count));
        let on_inspect_review = on_inspect.clone();
        let review_db = db.clone();
        items.push(PaletteItem::new(
            label,
            PaletteAction::Execute(Box::new(move |_filter, window, cx| {
                let page = crate::pending_edits::review_page(review_db.clone(), cx);
                on_inspect_review(page, window, cx);
            })),
        ));
    }

    items.push(PaletteItem::new(
        "Theme",
        PaletteAction::NextPage {
            label: Some("Theme".into()),
            page: Box::new(|| {
                let items: Vec<PaletteItem> = crate::theme::all_themes()
                    .iter()
                    .map(|t| {
                        let theme = Arc::new((*t).clone());
                        PaletteItem::new(
                            t.name,
                            PaletteAction::Execute(Box::new(move |_, _, cx| {
                                crate::theme::set_theme(cx, theme.clone());
                            })),
                        )
                    })
                    .collect();
                PalettePage::new(items).prompt("Select theme")
            }),
        },
    ));

    PalettePage::new(items).prompt("Command")
}

fn new_panel_page(
    db: Arc<DB>,
    pane: Entity<Pane>,
    on_inspect: Arc<dyn Fn(PalettePage, &mut Window, &mut App) + 'static>,
    on_open_inspector: Option<crate::inspector::OpenInspectorCallback>,
) -> PalettePage {
    let items = vec![
        PaletteItem::new("Time Series Plot", {
            let db = db.clone();
            let pane = pane.clone();
            let on_inspect = on_inspect.clone();
            let on_open_inspector = on_open_inspector.clone();
            PaletteAction::Execute(Box::new(move |_filter, window, cx| {
                let plot_panel = {
                    let db = db.clone();
                    cx.new(|cx| PlotPanel::empty(db, cx))
                };
                let inner = {
                    let panel = plot_panel.read(cx);
                    panel.inner().clone()
                };

                let cb: OpenPageCallback = Arc::new({
                    let on_inspect = on_inspect.clone();
                    move |page, window, cx| on_inspect(page, window, cx)
                });
                inner.update(cx, |plot, _cx| {
                    plot.set_on_open_page(cb);
                });

                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(plot_panel), cx);
                });

                // Auto-open the inspector so the user gets the trace wizard
                if let Some(on_open_inspector) = &on_open_inspector {
                    let inner_any = inner.into_any();
                    if let Some(rows) = crate::reflect::rows_for_any_entity(&inner_any, &db, cx) {
                        let request = crate::inspector::InspectorRequest {
                            rows,
                            mode: crate::inspector::InspectorMode::Centered,
                        };
                        on_open_inspector(request, window, cx);
                    }
                }
            }))
        }),
        PaletteItem::new("Component Text", {
            let db = db.clone();
            let pane = pane.clone();
            PaletteAction::NextPage {
                label: Some("Text".into()),
                page: Box::new(move || {
                    let db_outer = db.clone();
                    let pane = pane.clone();
                    component_picker_page(db.clone(), move |component_id, name, cx| {
                        let db = db_outer.clone();
                        pane.update(cx, |pane, cx| {
                            let item: Box<dyn PaneItemHandle> =
                                Box::new(cx.new(|cx| TextPanel::new(db, component_id, name, cx)));
                            pane.add_item(item, cx);
                        });
                    })
                }),
            }
        }),
        PaletteItem::new("Component Table", {
            let db = db.clone();
            let pane = pane.clone();
            PaletteAction::Execute(Box::new(move |_filter, _window, cx| {
                let db = db.clone();
                pane.update(cx, |pane, cx| {
                    let item: Box<dyn PaneItemHandle> =
                        Box::new(cx.new(|cx| TablePanel::new(db, cx)));
                    pane.add_item(item, cx);
                });
            }))
        }),
        PaletteItem::new("3D Viewer", {
            let db = db.clone();
            let pane = pane.clone();
            PaletteAction::Execute(Box::new(move |_filter, _window, cx| {
                let db = db.clone();
                pane.update(cx, |pane, cx| {
                    let item: Box<dyn PaneItemHandle> =
                        Box::new(cx.new(|cx| Viewer3dPanel::new(db, cx)));
                    pane.add_item(item, cx);
                });
            }))
        }),
        PaletteItem::new("Dashboard", {
            let db = db.clone();
            let pane = pane.clone();
            PaletteAction::Execute(Box::new(move |_filter, _window, cx| {
                let db = db.clone();
                pane.update(cx, |pane, cx| {
                    let dashboard = cx.new(|cx| DashboardPanel::new(db, cx));
                    pane.add_item(Box::new(dashboard), cx);
                });
            }))
        }),
    ];

    PalettePage::new(items).prompt("Select panel type")
}

/// Build a palette page listing available components, calling `on_select`
/// with the chosen component ID and name.
fn component_picker_page(
    db: Arc<DB>,
    on_select: impl Fn(ComponentId, String, &mut App) + 'static,
) -> PalettePage {
    let on_select = Arc::new(on_select);
    let items: Vec<PaletteItem> = crate::trace_picker::list_components(&db)
        .into_iter()
        .map(|(id, name)| {
            let on_select = on_select.clone();
            PaletteItem::new(
                SharedString::from(name.clone()),
                PaletteAction::Execute(Box::new(move |_filter, _window, cx| {
                    on_select(id, name.clone(), cx);
                })),
            )
        })
        .collect();

    PalettePage::new(items).prompt("Select component")
}

/// Build palette items for all panels across all panes.
fn collect_edit_labels(tiles: &Entity<super::TileGroup>, cx: &App) -> Vec<SharedString> {
    let panes = tiles.read(cx).panes().to_vec();
    let mut labels = Vec::new();

    for (pane_ix, pane) in panes.iter().enumerate() {
        let pane_items = pane.read(cx).items();
        for item in pane_items.iter() {
            let title = item.tab_title(cx);
            let label = if panes.len() > 1 {
                SharedString::from(format!("[Pane {}] {}", pane_ix + 1, title))
            } else {
                title
            };
            labels.push(label);
        }
    }

    labels
}
