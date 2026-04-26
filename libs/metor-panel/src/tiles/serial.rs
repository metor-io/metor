use std::collections::HashMap;

use gpui::{Context, Entity};
use serde::{Deserialize, Serialize};

use super::item::{PaneItem, PaneItemHandle};
use super::pane::TabOrientation;

/// On-disk form of a tile layout.
#[derive(Serialize, Deserialize)]
pub struct SerializedTileGroup {
    pub root: SerializedMember,
}

/// Node in a serialized tree; mirrors the runtime `Member` enum.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SerializedMember {
    Pane(SerializedPane),
    Split(SerializedSplit),
}

/// Persisted split node: axis, per-child flex weights, and recursive children.
#[derive(Serialize, Deserialize)]
pub struct SerializedSplit {
    pub axis: SerializedAxis,
    pub flexes: Vec<f32>,
    pub children: Vec<SerializedMember>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub enum SerializedAxis {
    Horizontal,
    Vertical,
}

impl From<gpui::Axis> for SerializedAxis {
    fn from(axis: gpui::Axis) -> Self {
        match axis {
            gpui::Axis::Horizontal => SerializedAxis::Horizontal,
            gpui::Axis::Vertical => SerializedAxis::Vertical,
        }
    }
}

impl From<SerializedAxis> for gpui::Axis {
    fn from(axis: SerializedAxis) -> Self {
        match axis {
            SerializedAxis::Horizontal => gpui::Axis::Horizontal,
            SerializedAxis::Vertical => gpui::Axis::Vertical,
        }
    }
}

/// Persisted pane: which tab was active plus the items themselves.
#[derive(Serialize, Deserialize)]
pub struct SerializedPane {
    pub active_index: usize,
    pub items: Vec<SerializedItem>,
    #[serde(default)]
    pub tab_orientation: TabOrientation,
    #[serde(default)]
    pub hide_tab_bar: bool,
    #[serde(default)]
    pub locked_size: Option<(f32, f32)>,
}

/// Persisted pane item, tagged with the [`PaneItem::serialization_key`] used
/// to pick a deserializer at load time.
#[derive(Serialize, Deserialize)]
pub struct SerializedItem {
    pub kind: String,
    pub state: serde_json::Value,
}

type DeserializeFn = Box<dyn Fn(serde_json::Value, &mut Context<super::pane::Pane>) -> Option<Box<dyn PaneItemHandle>>>;

/// Directory of `serialization_key -> constructor` used to rehydrate items.
///
/// Must be populated with every pane-item type before
/// [`TileGroup::deserialize`] is called, or the item is silently dropped.
pub struct ItemRegistry {
    deserializers: HashMap<String, DeserializeFn>,
}

impl Default for ItemRegistry {
    fn default() -> Self {
        Self {
            deserializers: HashMap::new(),
        }
    }
}

impl ItemRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Associate `T`'s serialization key with a constructor from JSON state.
    pub fn register<T: PaneItem>(
        &mut self,
        deserialize: impl Fn(serde_json::Value, &mut Context<super::pane::Pane>) -> Option<Entity<T>> + 'static,
    ) {
        self.deserializers.insert(
            T::serialization_key().to_string(),
            Box::new(move |value, cx| {
                let entity = deserialize(value, cx)?;
                Some(Box::new(entity) as Box<dyn PaneItemHandle>)
            }),
        );
    }

    /// Invoke the registered constructor for `kind`. Returns `None` when no
    /// type matches, so the caller can drop unknown items from the layout.
    pub fn deserialize(
        &self,
        kind: &str,
        state: serde_json::Value,
        cx: &mut Context<super::pane::Pane>,
    ) -> Option<Box<dyn PaneItemHandle>> {
        let f = self.deserializers.get(kind)?;
        f(state, cx)
    }
}
