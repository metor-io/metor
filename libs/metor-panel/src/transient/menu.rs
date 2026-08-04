//! The built-in chord menu tree.
//!
//! [`default_menu`] wires keys to tile operations and to the existing command
//! palette. Leaves either act on the tile group directly (split / close / cycle)
//! or hand off to the [`Inspector`](crate::inspector) by opening it with rows
//! pulled from the same builders the palette uses — so there is one source of
//! truth for "New tile", presets, theme, font, and edits.
use std::sync::Arc;

use gpui::{App, AppContext, Entity, Window};
use metor_db::DB;

use crate::inspector::palette::{
    ItemRegistry, font_rows, preset_load_rows, preset_save_rows, theme_rows,
};
use crate::inspector::rows::InspectorRow;
use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorCallback};
use crate::tiles::panels::new_panel_rows;
use crate::tiles::{Pane, SplitDirection, TileGroup};

use super::node::ChordNode;

/// The root chord menu: the levels reachable by the first keystroke after the
/// leader. Submenu children are built lazily so they reflect current state.
pub fn default_menu(
    db: Arc<DB>,
    tiles: Entity<TileGroup>,
    on_open_inspector: OpenInspectorCallback,
) -> Vec<ChordNode> {
    vec![
        ChordNode::submenu("w", "Window/tile", {
            let tiles = tiles.clone();
            let db = db.clone();
            let on_open = on_open_inspector.clone();
            move |_cx| window_menu(tiles.clone(), db.clone(), on_open.clone())
        }),
        ChordNode::submenu("t", "Tabs", {
            let tiles = tiles.clone();
            move |_cx| tabs_menu(tiles.clone())
        }),
        ChordNode::submenu("l", "Layout", {
            let tiles = tiles.clone();
            let on_open = on_open_inspector.clone();
            move |_cx| layout_menu(tiles.clone(), on_open.clone())
        }),
        ChordNode::submenu("a", "Appearance", {
            let on_open = on_open_inspector.clone();
            move |cx| appearance_menu(on_open.clone(), cx)
        }),
        ChordNode::submenu("e", "Edits", {
            let db = db.clone();
            let on_open = on_open_inspector.clone();
            move |_cx| edits_menu(db.clone(), on_open.clone())
        }),
        ChordNode::command("p", "Command palette", {
            let db = db.clone();
            let on_open = on_open_inspector.clone();
            move |window, cx| {
                let rows = ItemRegistry::root_rows(&db, cx);
                open_centered(&on_open, rows, window, cx);
            }
        }),
    ]
}

fn window_menu(
    tiles: Entity<TileGroup>,
    db: Arc<DB>,
    on_open: OpenInspectorCallback,
) -> Vec<ChordNode> {
    vec![
        ChordNode::command("v", "Split right", {
            let tiles = tiles.clone();
            let db = db.clone();
            let on_open = on_open.clone();
            move |window, cx| split_active(&tiles, SplitDirection::Right, &db, &on_open, window, cx)
        }),
        ChordNode::command("s", "Split down", {
            let tiles = tiles.clone();
            let db = db.clone();
            let on_open = on_open.clone();
            move |window, cx| split_active(&tiles, SplitDirection::Down, &db, &on_open, window, cx)
        }),
        ChordNode::command("n", "New tile…", {
            let tiles = tiles.clone();
            let db = db.clone();
            let on_open = on_open.clone();
            move |window, cx| {
                if let Some(pane) = tiles.read(cx).active_pane(cx) {
                    let rows = new_panel_rows(db.clone(), pane, Some(on_open.clone()), cx);
                    open_centered(&on_open, rows, window, cx);
                }
            }
        }),
        ChordNode::command("c", "Close tile", {
            let tiles = tiles.clone();
            move |_w, cx| close_active(&tiles, cx)
        }),
    ]
}

fn tabs_menu(tiles: Entity<TileGroup>) -> Vec<ChordNode> {
    vec![
        ChordNode::command("n", "Next tab", {
            let tiles = tiles.clone();
            move |_w, cx| cycle_active(&tiles, true, cx)
        }),
        ChordNode::command("p", "Prev tab", {
            let tiles = tiles.clone();
            move |_w, cx| cycle_active(&tiles, false, cx)
        }),
    ]
}

fn layout_menu(tiles: Entity<TileGroup>, on_open: OpenInspectorCallback) -> Vec<ChordNode> {
    vec![
        ChordNode::command("s", "Save preset", {
            let tiles = tiles.clone();
            let on_open = on_open.clone();
            move |window, cx| open_centered(&on_open, preset_save_rows(tiles.clone()), window, cx)
        }),
        ChordNode::command("l", "Load preset", {
            let tiles = tiles.clone();
            let on_open = on_open.clone();
            move |window, cx| {
                open_centered(&on_open, preset_load_rows(tiles.clone(), cx), window, cx)
            }
        }),
    ]
}

fn appearance_menu(on_open: OpenInspectorCallback, cx: &App) -> Vec<ChordNode> {
    let lock_label = if crate::inspector::edits::pending_edits(cx).locked {
        "Unlock editing"
    } else {
        "Lock editing"
    };
    vec![
        ChordNode::command("t", "Theme", {
            let on_open = on_open.clone();
            move |window, cx| open_centered(&on_open, theme_rows(), window, cx)
        }),
        ChordNode::command("f", "Font", {
            let on_open = on_open.clone();
            move |window, cx| {
                let rows = font_rows(cx);
                open_centered(&on_open, rows, window, cx);
            }
        }),
        ChordNode::command("l", lock_label, move |_w, cx| {
            let locked = !crate::inspector::edits::pending_edits(cx).locked;
            crate::inspector::edits::pending_edits_mut(cx).locked = locked;
        }),
    ]
}

fn edits_menu(db: Arc<DB>, on_open: OpenInspectorCallback) -> Vec<ChordNode> {
    vec![
        ChordNode::command("r", "Review edits", {
            let db = db.clone();
            let on_open = on_open.clone();
            move |window, cx| {
                let rows = crate::inspector::edits::review_rows(db.clone(), cx);
                open_centered(&on_open, rows, window, cx);
            }
        }),
        ChordNode::command("u", "Update component", {
            let db = db.clone();
            let on_open = on_open.clone();
            move |window, cx| {
                let rows = crate::inspector::edits::update_component_rows(db.clone());
                open_centered(&on_open, rows, window, cx);
            }
        }),
    ]
}

/// Open the inspector as a centered palette with `rows`.
fn open_centered(
    on_open: &OpenInspectorCallback,
    rows: Vec<Box<dyn InspectorRow>>,
    window: &mut Window,
    cx: &mut App,
) {
    on_open(
        InspectorRequest {
            rows,
            mode: InspectorMode::Centered,
        },
        window,
        cx,
    );
}

/// Split the active pane and immediately offer the new-tile palette for the
/// freshly-created (empty) pane, so a split flows straight into picking what
/// fills it.
fn split_active(
    tiles: &Entity<TileGroup>,
    direction: SplitDirection,
    db: &Arc<DB>,
    on_open: &OpenInspectorCallback,
    window: &mut Window,
    cx: &mut App,
) {
    let new_pane = tiles.update(cx, |tg, cx| {
        let target = tg.active_pane(cx)?;
        let new_pane = cx.new(|cx| Pane::new(vec![], cx));
        tg.split_pane(&target, new_pane.clone(), direction, cx);
        Some(new_pane)
    });
    if let Some(new_pane) = new_pane {
        let rows = new_panel_rows(db.clone(), new_pane, Some(on_open.clone()), cx);
        open_centered(on_open, rows, window, cx);
    }
}

fn close_active(tiles: &Entity<TileGroup>, cx: &mut App) {
    tiles.update(cx, |tg, cx| {
        // Keep at least one pane so the layout is never void.
        if tg.panes().len() > 1
            && let Some(target) = tg.active_pane(cx)
        {
            tg.remove_pane(&target, cx);
        }
    });
}

fn cycle_active(tiles: &Entity<TileGroup>, forward: bool, cx: &mut App) {
    if let Some(pane) = tiles.read(cx).active_pane(cx) {
        pane.update(cx, |pane, cx| {
            if forward {
                pane.cycle_forward(cx);
            } else {
                pane.cycle_backward(cx);
            }
        });
    }
}
