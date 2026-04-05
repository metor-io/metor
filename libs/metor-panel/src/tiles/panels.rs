use std::sync::Arc;

use gpui::{App, Context, Entity, IntoElement, Render, SharedString, Window, div, prelude::*};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::command_palette::{PaletteAction, PaletteItem, PalettePage};
use crate::elements::{ComponentTable, ComponentText, TimeSeriesPlot, new_component_table};
use crate::inspectable::{palette_page_for_inspectable, palette_page_for_field, FieldId, InspectionValue};

use super::item::{PaneItem, PaneItemHandle};
use super::pane::Pane;

// ── ComponentText wrapper ──────────────────────────────────────────

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

// ── ComponentTable wrapper ─────────────────────────────────────────

pub struct TablePanel {
    inner: Entity<ComponentTable>,
}

impl TablePanel {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| new_component_table(db, cx));
        Self { inner }
    }
}

impl Render for TablePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for TablePanel {
    fn tab_title(&self, _cx: &App) -> SharedString {
        "Components".into()
    }

    fn serialization_key() -> &'static str {
        "component_table"
    }

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({})
    }
}

// ── TimeSeriesPlot wrapper ─────────────────────────────────────────

pub struct PlotPanel {
    db: Arc<DB>,
    inner: Entity<TimeSeriesPlot>,
    label: SharedString,
}

impl PlotPanel {
    pub fn new(
        db: Arc<DB>,
        component_id: ComponentId,
        elements: &[usize],
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let inner =
            cx.new(|cx| TimeSeriesPlot::from_component(db.clone(), component_id, elements, cx));
        Self {
            db,
            inner,
            label: label.into(),
        }
    }

    /// Create an empty plot panel, ready to be configured via the inspector.
    pub fn empty(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let inner = cx.new(|cx| TimeSeriesPlot::new(db.clone(), vec![], cx));
        Self {
            db,
            inner,
            label: "Plot".into(),
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
}

impl Render for PlotPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

impl PaneItem for PlotPanel {
    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "time_series_plot"
    }

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({ "label": self.label.as_ref() })
    }

    fn inspect_page(&self, _db: Option<Arc<DB>>, cx: &App) -> Option<PalettePage> {
        Some(palette_page_for_inspectable(
            self.inner.clone(),
            Some(self.db.clone()),
            cx,
        ))
    }
}

// ── Command palette integration ────────────────────────────────────

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
    cx: &App,
) -> PalettePage {
    let on_inspect = Arc::new(on_inspect);

    let mut items = vec![
        PaletteItem::new("New Panel", {
            let db = db.clone();
            let pane = pane.clone();
            let on_inspect = on_inspect.clone();
            PaletteAction::NextPage {
                label: Some("New".into()),
                page: Box::new(move || new_panel_page(db, pane, on_inspect)),
            }
        }),
    ];

    // Only show "Edit Panel" if there are panels to edit
    let edit_items = build_edit_items(db.clone(), tiles, on_inspect.clone(), cx);
    if !edit_items.is_empty() {
        items.push(PaletteItem::new("Edit Panel", {
            PaletteAction::NextPage {
                label: Some("Edit".into()),
                page: Box::new(move || {
                    PalettePage::new(edit_items).prompt("Select panel to edit")
                }),
            }
        }));
    }

    PalettePage::new(items).prompt("Command")
}

fn new_panel_page(
    db: Arc<DB>,
    pane: Entity<Pane>,
    on_inspect: Arc<dyn Fn(PalettePage, &mut Window, &mut App) + 'static>,
) -> PalettePage {
    let items = vec![
        PaletteItem::new("Time Series Plot", {
            let db = db.clone();
            let pane = pane.clone();
            let on_inspect = on_inspect.clone();
            PaletteAction::Execute(Box::new(move |_filter, window, cx| {
                // Create an empty plot and add it to the pane
                let plot_panel = {
                    let db = db.clone();
                    cx.new(|cx| PlotPanel::empty(db, cx))
                };
                let inner = plot_panel.read(cx).inner().clone();
                let plot_db = plot_panel.read(cx).db().clone();

                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(plot_panel), cx);
                });

                // Go directly into the Traces field editor (component/element picker)
                let page = palette_page_for_field(
                    inner,
                    FieldId(0), // Traces field on TimeSeriesPlot
                    "Traces".into(),
                    InspectionValue::Traces(vec![]),
                    Some(plot_db),
                );
                on_inspect(page, window, cx);
            }))
        }),
        PaletteItem::new("Component Text", {
            let db = db.clone();
            let pane = pane.clone();
            PaletteAction::NextPage {
                label: Some("Text".into()),
                page: Box::new(move || {
                    component_picker_page(db.clone(), move |component_id, name, cx| {
                        let db = db.clone();
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
    let items: Vec<PaletteItem> = crate::inspectable::list_components(&db)
        .into_iter()
        .map(|(id, name)| {
            let on_select = on_select.clone();
            let name_clone = name.clone();
            PaletteItem::new(
                SharedString::from(name),
                PaletteAction::Execute(Box::new(move |_filter, _window, cx| {
                    on_select(id, name_clone, cx);
                })),
            )
        })
        .collect();

    PalettePage::new(items).prompt("Select component")
}

/// Build palette items for all panels across all panes.
fn build_edit_items(
    db: Arc<DB>,
    tiles: &Entity<super::TileGroup>,
    on_inspect: Arc<dyn Fn(PalettePage, &mut Window, &mut App) + 'static>,
    cx: &App,
) -> Vec<PaletteItem> {
    let panes = tiles.read(cx).panes().to_vec();
    let mut items = Vec::new();

    for (pane_ix, pane) in panes.iter().enumerate() {
        let pane_items = pane.read(cx).items();
        for item in pane_items.iter() {
            let title = item.tab_title(cx);
            let label = if panes.len() > 1 {
                SharedString::from(format!("[Pane {}] {}", pane_ix + 1, title))
            } else {
                title
            };

            let item_handle = item.clone_handle();
            let db = db.clone();
            let on_inspect = on_inspect.clone();
            items.push(PaletteItem::new(
                label,
                PaletteAction::Execute(Box::new(move |_filter, window, cx| {
                    if let Some(page) = item_handle.inspect_page(Some(db), cx) {
                        on_inspect(page, window, cx);
                    }
                })),
            ));
        }
    }

    items
}
