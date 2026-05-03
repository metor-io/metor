//! Palette entries for the node editor:
//!
//! - `Add Node` (submenu) appears when at least one [`NodeEditor`] pane is
//!   open. Selecting an op inserts a new node into the first such pane.
//! - The "Node Editor" entry under the existing `New Panel` submenu is
//!   added separately in [`crate::tiles::panels::new_panel_rows`].

use std::sync::Arc;

use gpui::{App, Entity, SharedString};

use crate::inspector::palette::{Category, InspectionItem, ItemProvider, ItemRegistry};
use crate::inspector::rows::{CommandRow, InspectorRow};
use crate::node_editor::pane::NodeEditor;
use crate::node_editor::registry as op_registry;
use crate::tiles::TileGroup;

pub fn register(tiles: Entity<TileGroup>, cx: &mut App) {
    let provider: ItemProvider = Arc::new(move |cx: &App| {
        let editors = find_editors(&tiles, cx);
        if editors.is_empty() {
            return Vec::new();
        }
        let target = editors[0].clone();
        vec![InspectionItem::SubMenu {
            label: "Add Node".into(),
            summary: SharedString::new_static(""),
            build: Arc::new(move |_cx| build_add_node_rows(target.clone())),
        }]
    });
    ItemRegistry::register(cx, Category::Custom("Node Editor".into()), provider);
}

fn find_editors(tiles: &Entity<TileGroup>, cx: &App) -> Vec<Entity<NodeEditor>> {
    let panes = tiles.read(cx).panes().to_vec();
    let mut out = Vec::new();
    for pane in &panes {
        for item in pane.read(cx).items().iter() {
            if let Ok(editor) = item.entity_any(cx).downcast::<NodeEditor>() {
                out.push(editor);
            }
        }
    }
    out
}

fn build_add_node_rows(target: Entity<NodeEditor>) -> Vec<Box<dyn InspectorRow>> {
    op_registry::ALL
        .iter()
        .map(|descriptor| {
            let target = target.clone();
            let label = SharedString::from(format!("{} · {}", descriptor.category, descriptor.label));
            Box::new(CommandRow::new(
                label,
                Arc::new(move |_window, cx| {
                    target.update(cx, |editor, cx| {
                        editor.add_node(descriptor, cx);
                    });
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}
