//! Palette entries for the node editor:
//!
//! Each open `NodeEditor` pane is exposed as a palette entry under the
//! `Node Editor` category. Selecting it drills into the editor's
//! [`InspectorRegistry`] type builder, which exposes "Add Node" (with a
//! wizard for `Persist`/`FromDb`) and "Delete" rows. The same builder is
//! reached via right-click on the editor tab and the editor surface, so
//! all three entry points share one menu.

use std::sync::Arc;

use gpui::{App, Entity, SharedString};

use crate::inspector::palette::{Category, InspectionItem, ItemProvider, ItemRegistry};
use crate::node_editor::pane::NodeEditor;
use crate::tiles::TileGroup;

pub fn register(tiles: Entity<TileGroup>, cx: &mut App) {
    let provider: ItemProvider = Arc::new(move |cx: &App| {
        find_editors(&tiles, cx)
            .into_iter()
            .enumerate()
            .map(|(ix, editor)| InspectionItem::Entity {
                label: SharedString::from(format!("Node Editor {}", ix + 1)),
                summary: SharedString::new_static(""),
                entity: editor.into_any(),
            })
            .collect()
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
