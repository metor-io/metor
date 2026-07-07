//! Pull-based registry that feeds the command palette.
//!
//! Each [`Category`] registers one [`ItemProvider`] closure. On every open,
//! the palette evaluates all providers and flattens the results into the top
//! page. Pull-based means no lifecycle bookkeeping: destroyed entities
//! simply stop appearing in the next snapshot.
use std::path::Path;
use std::sync::Arc;

use gpui::{AnyEntity, App, Entity, Global, PathPromptOptions, SharedString, Window};
use metor_db::DB;

use crate::config::FontConfig;
use crate::inspector::OpenInspectorCallback;
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow, NavRow};
use crate::presets;
use crate::tiles::TileGroup;
use crate::views::dashboard::DashboardPanel;

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

fn item_to_row(item: InspectionItem, db: Arc<DB>, tag: SharedString) -> Box<dyn InspectorRow> {
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
                    crate::inspector::reflect::rows_for_any_entity(&entity, &db, cx)
                        .unwrap_or_default()
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
        } => Box::new(NavRow::new(label, summary, Box::new(move |cx| build(cx))).with_tag(tag)),
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
    register_pane_settings_provider(tiles.clone(), cx);
    register_widget_provider(tiles.clone(), cx);
    register_preset_provider(tiles.clone(), cx);
    register_command_provider(db, tiles, on_open_inspector, cx);
}

/// Reachable even when the tab bar is hidden.
fn register_pane_settings_provider(tiles: Entity<TileGroup>, cx: &mut App) {
    let provider: ItemProvider = Arc::new(move |cx: &App| {
        let panes = tiles.read(cx).panes().to_vec();
        panes
            .iter()
            .enumerate()
            .map(|(ix, pane)| {
                let label = match pane.read(cx).items().first() {
                    Some(item) => SharedString::from(format!("Pane: {}", item.tab_title(cx))),
                    None => SharedString::from(format!("Pane {}", ix + 1)),
                };
                InspectionItem::Entity {
                    label,
                    summary: SharedString::new_static(""),
                    entity: pane.clone().into_any(),
                }
            })
            .collect()
    });
    ItemRegistry::register(cx, Category::Custom("Pane".into()), provider);
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

        // New Panel targets the current pane so palette-created panels land in
        // the tile the user last interacted with (falling back to the first).
        if let Some(pane) = tiles.read(cx).active_pane(cx) {
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
        }

        let update_db = db.clone();
        items.push(InspectionItem::SubMenu {
            label: "Update Component".into(),
            summary: SharedString::new_static(""),
            build: Arc::new(move |_cx| {
                crate::inspector::edits::update_component_rows(update_db.clone())
            }),
        });

        let plot_db = db.clone();
        items.push(InspectionItem::SubMenu {
            label: "Plot Component".into(),
            summary: SharedString::new_static(
                "Pick traces \u{2192} preview without leaving the palette",
            ),
            build: Arc::new(move |_cx| {
                let preview_db = plot_db.clone();
                crate::inspector::trace_picker::select_traces_wizard_view(
                    plot_db.clone(),
                    Arc::new(|_| 0),
                    Arc::new(move |traces, cx| {
                        crate::inspector::plot_preview::build_plot_preview(
                            preview_db.clone(),
                            traces,
                            cx,
                        )
                    }),
                )
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
            build: Arc::new(|_cx| theme_rows()),
        });

        items.push(InspectionItem::SubMenu {
            label: "Font".into(),
            summary: SharedString::new_static("Pick the UI font"),
            build: Arc::new(font_rows),
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

/// Read a preset file and apply it to `tiles`. Errors are logged and the
/// layout is left untouched, so a bad file can't take the app down.
fn read_and_load(path: &Path, tiles: &Entity<TileGroup>, cx: &mut App) {
    match presets::load_preset(path) {
        Ok(json) => presets::load_into_tiles(&json, tiles, cx),
        Err(e) => tracing::error!(path = %path.display(), %e, "read preset failed"),
    }
}

/// Register the "Preset" category with two entries: **Save preset** and
/// **Load preset**. Each opens a submenu listing the built-in directory
/// items alongside a `Save to file…` / `Load from file…` row, so the OS
/// file dialog is reachable without a separate top-level command.
fn register_preset_provider(tiles: Entity<TileGroup>, cx: &mut App) {
    let provider: ItemProvider = Arc::new(move |_cx: &App| {
        let mut items: Vec<InspectionItem> = Vec::new();

        // Save preset
        let tiles_for_save = tiles.clone();
        items.push(InspectionItem::SubMenu {
            label: SharedString::new_static("Save preset"),
            summary: SharedString::new_static("Type a name, or pick \"Save to file…\""),
            build: Arc::new(move |_cx| preset_save_rows(tiles_for_save.clone())),
        });

        // Load preset
        let tiles_for_load = tiles.clone();
        items.push(InspectionItem::SubMenu {
            label: SharedString::new_static("Load preset"),
            summary: SharedString::new_static(""),
            build: Arc::new(move |_cx| preset_load_rows(tiles_for_load.clone())),
        });

        items
    });
    ItemRegistry::register(cx, Category::Custom("Preset".into()), provider);
}

/// Rows for the **Theme** submenu: one per registered theme, applied on select.
/// Shared by the command palette and the transient chord menu.
pub(crate) fn theme_rows() -> Vec<Box<dyn InspectorRow>> {
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
}

/// Rows for the **Font** submenu: an `Auto` entry plus every installed family.
pub(crate) fn font_rows(cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    rows.push(Box::new(CommandRow::new(
        SharedString::new_static("Auto (Berkeley Mono, else bundled)"),
        Arc::new(move |_window, cx| {
            crate::theme::set_font(cx, FontConfig::Auto);
        }),
    )));
    for name in cx.text_system().all_font_names() {
        rows.push(Box::new(CommandRow::new(
            name.clone(),
            Arc::new(move |_window, cx| {
                crate::theme::set_font(cx, FontConfig::Family(name.clone()));
            }),
        )) as Box<dyn InspectorRow>);
    }
    rows
}

/// Rows for the **Save preset** submenu: a name prompt plus a "Save to file…"
/// OS dialog. Shared by the command palette and the transient chord menu.
pub(crate) fn preset_save_rows(tiles: Entity<TileGroup>) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    let tiles_for_name = tiles.clone();
    rows.push(Box::new(DefaultActionRow {
        label: SharedString::new_static("Type a name and press Enter"),
        callback: Arc::new(move |name, _window, cx| {
            let json = tiles_for_name.read(cx).to_json(cx);
            if let Err(e) = presets::save_preset(&name, &json) {
                tracing::error!(%name, %e, "save preset failed");
            }
        }),
    }));
    rows.push(Box::new(CommandRow::new(
        SharedString::new_static("Save to file…"),
        Arc::new(move |_window, cx| {
            let initial_dir = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
            let receiver = cx.prompt_for_new_path(&initial_dir, Some("metor-layout.json"));
            let tiles = tiles.clone();
            // Snapshot the layout *after* the user confirms the path,
            // so any in-flight edits land in the saved file.
            cx.spawn(async move |cx| {
                let Ok(Ok(Some(path))) = receiver.await else {
                    return;
                };
                let json = match cx.update(|cx| tiles.read(cx).to_json(cx)) {
                    Ok(j) => j,
                    Err(_) => return,
                };
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::error!(path = %path.display(), %e, "save preset failed");
                }
            })
            .detach();
        }),
    )));
    rows
}

/// Rows for the **Load preset** submenu: the saved presets plus a "Load from
/// file…" OS dialog. Shared by the command palette and the transient chord menu.
pub(crate) fn preset_load_rows(tiles: Entity<TileGroup>) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    for (name, path) in presets::list_presets() {
        let tiles = tiles.clone();
        rows.push(Box::new(CommandRow::new(
            SharedString::from(name),
            Arc::new(move |_window, cx| read_and_load(&path, &tiles, cx)),
        )));
    }
    rows.push(Box::new(CommandRow::new(
        SharedString::new_static("Load from file…"),
        Arc::new(move |_window, cx| {
            let receiver = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(SharedString::new_static("Open layout preset")),
            });
            let tiles = tiles.clone();
            cx.spawn(async move |cx| {
                let Ok(Ok(Some(paths))) = receiver.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let _ = cx.update(|cx| read_and_load(&path, &tiles, cx));
            })
            .detach();
        }),
    )));
    rows
}
