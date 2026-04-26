use std::collections::HashMap;

use gpui::{Context, Entity};

use super::item::{PaneItem, PaneItemHandle};
use super::pane::TabOrientation;

/// On-disk form of a tile layout.
#[derive(facet::Facet)]
pub struct SerializedTileGroup {
    /// Bumped only on a structural break in the document. Field-level changes
    /// rely on Facet's `#[facet(default)]` instead.
    pub version: u32,
    pub root: SerializedMember,
}

/// Node in a serialized tree; mirrors the runtime `Member` enum.
#[derive(facet::Facet)]
#[repr(u8)]
pub enum SerializedMember {
    Pane(SerializedPane),
    Split(SerializedSplit),
}

/// Persisted split node: axis, per-child flex weights, and recursive children.
#[derive(facet::Facet)]
pub struct SerializedSplit {
    pub axis: SerializedAxis,
    pub flexes: Vec<f32>,
    pub children: Vec<SerializedMember>,
}

#[derive(facet::Facet, Clone, Copy)]
#[repr(u8)]
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
#[derive(facet::Facet)]
pub struct SerializedPane {
    pub active_index: usize,
    pub tab_orientation: TabOrientation,
    pub hide_tab_bar: bool,
    pub locked_size: Option<(f32, f32)>,
    pub items: Vec<SerializedItem>,
}

/// Persisted pane item, tagged with the [`PaneItem::serialization_key`] used
/// to pick a deserializer at load time. `state` is a `facet-json` blob the
/// owning panel produced via its own `*Config` struct.
#[derive(facet::Facet)]
pub struct SerializedItem {
    pub kind: String,
    pub state: String,
}

type DeserializeFn =
    Box<dyn Fn(&str, &mut Context<super::pane::Pane>) -> Option<Box<dyn PaneItemHandle>>>;

/// Directory of `serialization_key -> constructor` used to rehydrate items.
///
/// Must be populated with every pane-item type before
/// [`super::TileGroup::deserialize`] is called, or the item is silently dropped.
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

    /// Associate `T`'s serialization key with a constructor from a facet-json
    /// blob. Each panel decides what shape to expect inside `state`; the
    /// closure is responsible for parsing it (typically with
    /// `facet_json::from_str` into a `*Config` struct).
    pub fn register<T: PaneItem>(
        &mut self,
        deserialize: impl Fn(&str, &mut Context<super::pane::Pane>) -> Option<Entity<T>> + 'static,
    ) {
        self.deserializers.insert(
            T::serialization_key().to_string(),
            Box::new(move |state, cx| {
                let entity = deserialize(state, cx)?;
                Some(Box::new(entity) as Box<dyn PaneItemHandle>)
            }),
        );
    }

    /// Invoke the registered constructor for `kind`. Returns `None` when no
    /// type matches, so the caller can drop unknown items from the layout.
    pub fn deserialize(
        &self,
        kind: &str,
        state: &str,
        cx: &mut Context<super::pane::Pane>,
    ) -> Option<Box<dyn PaneItemHandle>> {
        let f = self.deserializers.get(kind)?;
        f(state, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::pane::TabOrientation;

    fn sample_layout() -> SerializedTileGroup {
        SerializedTileGroup {
            version: 1,
            root: SerializedMember::Split(SerializedSplit {
                axis: SerializedAxis::Horizontal,
                flexes: vec![1.0, 2.0],
                children: vec![
                    SerializedMember::Pane(SerializedPane {
                        active_index: 0,
                        tab_orientation: TabOrientation::Horizontal,
                        hide_tab_bar: false,
                        locked_size: None,
                        items: vec![SerializedItem {
                            kind: "component_text".into(),
                            state: r#"{"component":"foo"}"#.into(),
                        }],
                    }),
                    SerializedMember::Pane(SerializedPane {
                        active_index: 1,
                        tab_orientation: TabOrientation::Vertical,
                        hide_tab_bar: true,
                        locked_size: Some((300.0, 200.0)),
                        items: vec![
                            SerializedItem {
                                kind: "component_table".into(),
                                state: "{}".into(),
                            },
                            SerializedItem {
                                kind: "time_series_plot".into(),
                                state: r#"{"label":"speed"}"#.into(),
                            },
                        ],
                    }),
                ],
            }),
        }
    }

    #[test]
    fn round_trip_preserves_tree_shape() {
        let original = sample_layout();
        let json = facet_json::to_string(&original).expect("serialize");
        let parsed: SerializedTileGroup = facet_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.version, 1);
        let SerializedMember::Split(split) = parsed.root else {
            panic!("expected split");
        };
        assert!(matches!(split.axis, SerializedAxis::Horizontal));
        assert_eq!(split.flexes, vec![1.0, 2.0]);
        assert_eq!(split.children.len(), 2);

        let SerializedMember::Pane(p0) = &split.children[0] else {
            panic!("expected pane");
        };
        assert_eq!(p0.active_index, 0);
        assert!(matches!(p0.tab_orientation, TabOrientation::Horizontal));
        assert!(!p0.hide_tab_bar);
        assert_eq!(p0.locked_size, None);
        assert_eq!(p0.items.len(), 1);
        assert_eq!(p0.items[0].kind, "component_text");
        assert_eq!(p0.items[0].state, r#"{"component":"foo"}"#);

        let SerializedMember::Pane(p1) = &split.children[1] else {
            panic!("expected pane");
        };
        assert_eq!(p1.active_index, 1);
        assert!(matches!(p1.tab_orientation, TabOrientation::Vertical));
        assert!(p1.hide_tab_bar);
        assert_eq!(p1.locked_size, Some((300.0, 200.0)));
        assert_eq!(p1.items.len(), 2);
    }
}
