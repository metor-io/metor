use std::collections::HashMap;

use gpui::{Context, Entity};
use serde::{Deserialize, Serialize};

use super::item::{PaneItem, PaneItemHandle};

#[derive(Serialize, Deserialize)]
pub struct SerializedTileGroup {
    pub root: SerializedMember,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SerializedMember {
    Pane(SerializedPane),
    Split(SerializedSplit),
}

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

#[derive(Serialize, Deserialize)]
pub struct SerializedPane {
    pub active_index: usize,
    pub items: Vec<SerializedItem>,
}

#[derive(Serialize, Deserialize)]
pub struct SerializedItem {
    pub kind: String,
    pub state: serde_json::Value,
}

/// Type-erased deserializer function.
type DeserializeFn = Box<dyn Fn(serde_json::Value, &mut Context<super::pane::Pane>) -> Option<Box<dyn PaneItemHandle>>>;

/// Registry mapping serialization keys to item deserializers.
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

    /// Register a deserializer for a PaneItem type.
    /// The closure receives the serialized JSON state and should return an Entity.
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

    /// Deserialize an item by its kind key.
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
