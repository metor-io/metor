use std::sync::Arc;

use gpui::{App, Context, Entity, IntoElement, Render, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::command_palette::{PaletteAction, PaletteItem, PalettePage};
use crate::elements::{ComponentTable, ComponentText, TimeSeriesPlot, new_component_table};
use crate::inspectable::element_names_for_component;
use crate::theme::DARK;

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
    inner: Entity<TimeSeriesPlot>,
    label: SharedString,
}

impl PlotPanel {
    pub fn new(
        db: Arc<DB>,
        component_id: ComponentId,
        elements: Vec<usize>,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let inner = cx.new(|cx| TimeSeriesPlot::from_component(db, component_id, elements, cx));
        Self {
            inner,
            label: label.into(),
        }
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
        serde_json::json!({ "component": self.label.as_ref() })
    }
}

// ── Command palette integration ────────────────────────────────────

/// Build a palette page that lets the user spawn new panel items
/// into the given pane. Requires a DB for component listing.
pub fn new_panel_palette_page(db: Arc<DB>, pane: Entity<Pane>) -> PalettePage {
    let items = vec![
        PaletteItem::new("Time Series Plot", {
            let db = db.clone();
            let pane = pane.clone();
            PaletteAction::NextPage {
                label: Some("Plot".into()),
                page: Box::new(move || {
                    plot_component_picker_page(db, pane)
                }),
            }
        }),
        PaletteItem::new("Component Text", {
            let db = db.clone();
            let pane = pane.clone();
            PaletteAction::NextPage {
                label: Some("Text".into()),
                page: Box::new(move || {
                    let db2 = db.clone();
                    let pane2 = pane.clone();
                    component_picker_page(db.clone(), move |component_id, name, cx| {
                        spawn_text(db2.clone(), pane2.clone(), component_id, name, cx);
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

/// Build a palette page listing components, then execute `on_select` with the chosen one.
fn component_picker_page(
    db: Arc<DB>,
    on_select: impl Fn(ComponentId, String, &mut App) + 'static,
) -> PalettePage {
    let mut components: Vec<(ComponentId, String)> = db.with_state(|state| {
        state
            .component_metadata_iter()
            .map(|(id, meta)| (*id, meta.name.clone()))
            .collect()
    });
    components.sort_by(|a, b| a.1.cmp(&b.1));

    let on_select = Arc::new(on_select);
    let items: Vec<PaletteItem> = components
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

/// Build a palette page listing elements of a component, with an "All" option.
fn element_picker_page(
    db: Arc<DB>,
    component_id: ComponentId,
    component_name: String,
    on_select: impl Fn(Vec<usize>, &mut App) + 'static,
) -> PalettePage {
    let element_names = element_names_for_component(&db, component_id);
    let elem_count = element_names.len().max(1);

    // For scalar components (single element), skip the picker entirely
    if elem_count <= 1 {
        let mut page = PalettePage::new(vec![]);
        // Immediately select element 0 — but we can't execute here, so make a single-item page
        let on_select = Arc::new(on_select);
        let item = PaletteItem::new(
            component_name.clone(),
            PaletteAction::Execute(Box::new(move |_, _, cx| {
                on_select(vec![0], cx);
            })),
        );
        page.items.push(item);
        return page.prompt("Scalar component — press Enter");
    }

    let on_select = Arc::new(on_select);
    let mut items = Vec::new();

    // "All" option
    {
        let on_select = on_select.clone();
        let all_indices: Vec<usize> = (0..elem_count).collect();
        items.push(PaletteItem::new(
            "All",
            PaletteAction::Execute(Box::new(move |_, _, cx| {
                on_select(all_indices, cx);
            })),
        ));
    }

    // Individual elements
    for (i, elem_name) in element_names.iter().enumerate() {
        let on_select = on_select.clone();
        let display = if elem_name.is_empty() {
            format!("[{}]", i)
        } else {
            elem_name.clone()
        };
        items.push(PaletteItem::new(
            SharedString::from(display),
            PaletteAction::Execute(Box::new(move |_, _, cx| {
                on_select(vec![i], cx);
            })),
        ));
    }

    PalettePage::new(items)
        .label(component_name)
        .prompt("Select element")
}

/// Component picker for plots: each component leads to an element picker page.
fn plot_component_picker_page(db: Arc<DB>, pane: Entity<Pane>) -> PalettePage {
    let mut components: Vec<(ComponentId, String)> = db.with_state(|state| {
        state
            .component_metadata_iter()
            .map(|(id, meta)| (*id, meta.name.clone()))
            .collect()
    });
    components.sort_by(|a, b| a.1.cmp(&b.1));

    let items: Vec<PaletteItem> = components
        .into_iter()
        .map(|(id, name)| {
            let db = db.clone();
            let pane = pane.clone();
            let display_name = name.clone();
            PaletteItem::new(
                SharedString::from(display_name),
                PaletteAction::NextPage {
                    label: None,
                    page: Box::new(move || {
                        let db2 = db.clone();
                        let pane2 = pane.clone();
                        let name2 = name.clone();
                        element_picker_page(db, id, name, move |elements, cx| {
                            spawn_plot(db2.clone(), pane2.clone(), id, name2.clone(), elements, cx);
                        })
                    }),
                },
            )
        })
        .collect();

    PalettePage::new(items).prompt("Select component")
}

fn spawn_text(db: Arc<DB>, pane: Entity<Pane>, component_id: ComponentId, name: String, cx: &mut App) {
    pane.update(cx, |pane, cx| {
        let item: Box<dyn PaneItemHandle> =
            Box::new(cx.new(|cx| TextPanel::new(db, component_id, name, cx)));
        pane.add_item(item, cx);
    });
}

fn spawn_plot(db: Arc<DB>, pane: Entity<Pane>, component_id: ComponentId, name: String, elements: Vec<usize>, cx: &mut App) {
    pane.update(cx, |pane, cx| {
        let item: Box<dyn PaneItemHandle> =
            Box::new(cx.new(|cx| PlotPanel::new(db, component_id, elements, name, cx)));
        pane.add_item(item, cx);
    });
}
