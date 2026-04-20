//! Pull-based registry that feeds the command palette.
//!
//! Each [`Category`] registers one [`ItemProvider`] closure. On every open,
//! the palette evaluates all providers and flattens the results into the top
//! page. Pull-based means no lifecycle bookkeeping: destroyed entities
//! simply stop appearing in the next snapshot.
use std::sync::Arc;

use gpui::{AnyEntity, App, Entity, Global, SharedString, Window};
use metor_db::DB;

use crate::inspector::OpenInspectorCallback;
use crate::tiles::TileGroup;
use crate::views::dashboard::DashboardPanel;
use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};

/// Tag shown as a pill alongside each palette row, also used to group
/// providers. `Custom` lets external subsystems add categories without
/// editing this enum.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Category {
    Panel,
    Widget,
    Command,
    Custom(SharedString),
}

impl Category {
    pub fn label(&self) -> SharedString {
        match self {
            Self::Panel => "Panel".into(),
            Self::Widget => "Widget".into(),
            Self::Command => "Command".into(),
            Self::Custom(name) => name.clone(),
        }
    }
}

/// One selectable entry returned by an [`ItemProvider`].
pub enum InspectionItem {
    /// Drill into an entity's reflected properties.
    Entity {
        label: SharedString,
        summary: SharedString,
        entity: AnyEntity,
    },
    /// Run a callback and dismiss.
    Command {
        label: SharedString,
        callback: Arc<dyn Fn(&mut Window, &mut App)>,
    },
    /// Push another page of rows, built lazily on selection.
    SubMenu {
        label: SharedString,
        summary: SharedString,
        build: Arc<dyn Fn(&App) -> Vec<Box<dyn InspectorRow>>>,
    },
}

/// Closure run on every palette open to snapshot a category's items.
pub type ItemProvider = Arc<dyn Fn(&App) -> Vec<InspectionItem>>;

/// Global holding the category-keyed provider list.
pub struct ItemRegistry {
    providers: Vec<(Category, ItemProvider)>,
}

impl Global for ItemRegistry {}

impl ItemRegistry {
    pub fn init(cx: &mut App) {
        cx.set_global(Self {
            providers: Vec::new(),
        });
    }

    pub fn register(cx: &mut App, category: Category, provider: ItemProvider) {
        cx.global_mut::<Self>().providers.push((category, provider));
    }

    /// Evaluate every provider and return its fresh items.
    pub fn snapshot(cx: &App) -> Vec<(Category, Vec<InspectionItem>)> {
        cx.global::<Self>()
            .providers
            .iter()
            .map(|(c, p)| (c.clone(), p(cx)))
            .collect()
    }

    /// Build the row list the palette renders on open. Rows carry their
    /// category as a tag pill.
    pub fn root_rows(db: &Arc<DB>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
        let snapshot = Self::snapshot(cx);
        let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
        for (category, items) in snapshot {
            let tag = category.label();
            for item in items {
                rows.push(item_to_row(item, db.clone(), tag.clone()));
            }
        }
        rows
    }
}

fn item_to_row(
    item: InspectionItem,
    db: Arc<DB>,
    tag: SharedString,
) -> Box<dyn InspectorRow> {
    match item {
        InspectionItem::Entity {
            label,
            summary,
            entity,
        } => Box::new(
            NavRow::new(
                label,
                summary,
                Box::new(move |cx| {
                    crate::inspector::reflect::rows_for_any_entity(&entity, &db, cx).unwrap_or_default()
                }),
            )
            .with_tag(tag),
        ),
        InspectionItem::Command { label, callback } => {
            Box::new(CommandRow::new(label, callback).with_tag(tag))
        }
        InspectionItem::SubMenu {
            label,
            summary,
            build,
        } => Box::new(
            NavRow::new(
                label,
                summary,
                Box::new(move |cx| build(cx)),
            )
            .with_tag(tag),
        ),
    }
}

/// Register the built-in Panel, Widget, and Command providers.
///
/// Call once after [`ItemRegistry::init`]. `on_open_inspector` is forwarded
/// to the "New Panel → Time Series Plot" flow so the trace wizard can open
/// immediately after the plot is created.
pub fn register_builtin_providers(
    db: Arc<DB>,
    tiles: Entity<TileGroup>,
    on_open_inspector: OpenInspectorCallback,
    cx: &mut App,
) {
    register_panel_provider(tiles.clone(), cx);
    register_widget_provider(tiles.clone(), cx);
    register_command_provider(db, tiles, on_open_inspector, cx);
}

fn register_panel_provider(tiles: Entity<TileGroup>, cx: &mut App) {
    let provider: ItemProvider = Arc::new(move |cx: &App| {
        let panes = tiles.read(cx).panes().to_vec();
        let multi_pane = panes.len() > 1;
        let mut items = Vec::new();
        for (pane_ix, pane) in panes.iter().enumerate() {
            let pane_items = pane.read(cx).items();
            for item in pane_items.iter() {
                let label = item.tab_title(cx);
                let summary = if multi_pane {
                    SharedString::from(format!("Pane {}", pane_ix + 1))
                } else {
                    SharedString::new_static("")
                };
                items.push(InspectionItem::Entity {
                    label,
                    summary,
                    entity: item.entity_any(cx),
                });
            }
        }
        items
    });
    ItemRegistry::register(cx, Category::Panel, provider);
}

fn register_widget_provider(tiles: Entity<TileGroup>, cx: &mut App) {
    let provider: ItemProvider = Arc::new(move |cx: &App| {
        let panes = tiles.read(cx).panes().to_vec();
        let mut items = Vec::new();
        for pane in panes.iter() {
            let pane_items = pane.read(cx).items();
            for item in pane_items.iter() {
                let entity = item.entity_any(cx);
                let Ok(dashboard) = entity.downcast::<DashboardPanel>() else {
                    continue;
                };
                let dashboard_title = dashboard.read(cx).title();
                for (_id, widget_entity, label) in dashboard.read(cx).inspectable_widgets(cx) {
                    items.push(InspectionItem::Entity {
                        label,
                        summary: dashboard_title.clone(),
                        entity: widget_entity,
                    });
                }
            }
        }
        items
    });
    ItemRegistry::register(cx, Category::Widget, provider);
}

fn register_command_provider(
    db: Arc<DB>,
    tiles: Entity<TileGroup>,
    on_open_inspector: OpenInspectorCallback,
    cx: &mut App,
) {
    let provider: ItemProvider = Arc::new(move |cx: &App| {
        let mut items: Vec<InspectionItem> = Vec::new();

        // New Panel targets the first pane so palette-created panels always
        // land somewhere visible, even when multiple panes are open.
        let pane = tiles.read(cx).panes()[0].clone();
        let new_db = db.clone();
        let new_open = on_open_inspector.clone();
        items.push(InspectionItem::SubMenu {
            label: "New Panel".into(),
            summary: SharedString::new_static(""),
            build: Arc::new(move |_cx| {
                crate::tiles::panels::new_panel_rows(
                    new_db.clone(),
                    pane.clone(),
                    Some(new_open.clone()),
                )
            }),
        });

        let update_db = db.clone();
        items.push(InspectionItem::SubMenu {
            label: "Update Component".into(),
            summary: SharedString::new_static(""),
            build: Arc::new(move |_cx| {
                crate::inspector::edits::update_component_rows(update_db.clone())
            }),
        });

        let pending_count = crate::inspector::edits::pending_edits(cx).edits.len();
        if pending_count > 0 {
            let review_db = db.clone();
            let review_open = on_open_inspector.clone();
            items.push(InspectionItem::Command {
                label: SharedString::from(format!("Review Edits ({})", pending_count)),
                callback: Arc::new(move |window, cx| {
                    let rows = crate::inspector::edits::review_rows(review_db.clone(), cx);
                    review_open(
                        crate::inspector::InspectorRequest {
                            rows,
                            mode: crate::inspector::InspectorMode::Centered,
                        },
                        window,
                        cx,
                    );
                }),
            });
        }

        items.push(InspectionItem::SubMenu {
            label: "Theme".into(),
            summary: SharedString::new_static(""),
            build: Arc::new(|_cx| {
                crate::theme::all_themes()
                    .iter()
                    .map(|t| {
                        let theme = Arc::new((*t).clone());
                        Box::new(CommandRow::new(
                            t.name,
                            Arc::new(move |_window, cx| {
                                crate::theme::set_theme(cx, theme.clone());
                            }),
                        )) as Box<dyn InspectorRow>
                    })
                    .collect()
            }),
        });

        items.push(InspectionItem::Command {
            label: if crate::inspector::edits::pending_edits(cx).locked {
                "Unlock Editing".into()
            } else {
                "Lock Editing".into()
            },
            callback: Arc::new(|_window, cx| {
                let locked = !crate::inspector::edits::pending_edits(cx).locked;
                crate::inspector::edits::pending_edits_mut(cx).locked = locked;
            }),
        });

        items
    });
    ItemRegistry::register(cx, Category::Command, provider);
}
