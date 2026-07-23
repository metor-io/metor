use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Context, Entity, Global};

pub use metor_proto_wkt::{
    SplitAxis, TabOrientation, TILE_LAYOUT_VERSION, TileItem, TileLayout, TileNode, TilePane,
    TileSplit,
};

use super::item::{PaneItem, PaneItemHandle};

// `SplitAxis` and `gpui::Axis` are both foreign here, so the conversions are
// free functions rather than `From` impls.
pub(crate) fn split_axis(axis: gpui::Axis) -> SplitAxis {
    match axis {
        gpui::Axis::Horizontal => SplitAxis::Horizontal,
        gpui::Axis::Vertical => SplitAxis::Vertical,
    }
}

pub(crate) fn gpui_axis(axis: SplitAxis) -> gpui::Axis {
    match axis {
        SplitAxis::Horizontal => gpui::Axis::Horizontal,
        SplitAxis::Vertical => gpui::Axis::Vertical,
    }
}

type DeserializeFn =
    Arc<dyn Fn(&str, &mut Context<super::pane::Pane>) -> Option<Box<dyn PaneItemHandle>>>;

/// Directory of `serialization_key -> constructor` used to rehydrate items.
///
/// Cloning is cheap (deserializers live behind an `Arc<HashMap>`), so callers
/// can snapshot the global, drop the borrow on `cx`, and still deserialize
/// items via `&mut Context`.
///
/// Must be populated with every pane-item type before
/// [`super::TileGroup::deserialize`] is called, or the item is silently dropped.
#[derive(Clone, Default)]
pub struct ItemRegistry {
    deserializers: Arc<HashMap<String, DeserializeFn>>,
}

impl Global for ItemRegistry {}

impl ItemRegistry {
    /// Associate `T`'s serialization key with a constructor from a JSON
    /// blob. Each panel decides what shape to expect inside `state`; the
    /// closure is responsible for parsing it (typically with
    /// `serde_json::from_str` into a `*Config` struct).
    pub fn register<T: PaneItem>(
        &mut self,
        deserialize: impl Fn(&str, &mut Context<super::pane::Pane>) -> Option<Entity<T>> + 'static,
    ) {
        let map = Arc::make_mut(&mut self.deserializers);
        map.insert(
            T::serialization_key().to_string(),
            Arc::new(move |state, cx| {
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

    fn sample_layout() -> TileLayout {
        TileLayout {
            version: 1,
            global_time_range: String::new(),
            root: TileNode::Split(TileSplit {
                axis: SplitAxis::Horizontal,
                flexes: vec![1.0, 2.0],
                children: vec![
                    TileNode::Pane(TilePane {
                        active_index: 0,
                        tab_orientation: TabOrientation::Horizontal,
                        hide_tab_bar: false,
                        locked_size: None,
                        items: vec![TileItem {
                            kind: "component_text".into(),
                            state: r#"{"component":"foo"}"#.into(),
                        }],
                    }),
                    TileNode::Pane(TilePane {
                        active_index: 1,
                        tab_orientation: TabOrientation::Vertical,
                        hide_tab_bar: true,
                        locked_size: Some((300.0, 200.0)),
                        items: vec![
                            TileItem {
                                kind: "component_table".into(),
                                state: "{}".into(),
                            },
                            TileItem {
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
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: TileLayout = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.version, 1);
        let TileNode::Split(split) = parsed.root else {
            panic!("expected split");
        };
        assert!(matches!(split.axis, SplitAxis::Horizontal));
        assert_eq!(split.flexes, vec![1.0, 2.0]);
        assert_eq!(split.children.len(), 2);

        let TileNode::Pane(p0) = &split.children[0] else {
            panic!("expected pane");
        };
        assert_eq!(p0.active_index, 0);
        assert!(matches!(p0.tab_orientation, TabOrientation::Horizontal));
        assert!(!p0.hide_tab_bar);
        assert_eq!(p0.locked_size, None);
        assert_eq!(p0.items.len(), 1);
        assert_eq!(p0.items[0].kind, "component_text");
        assert_eq!(p0.items[0].state, r#"{"component":"foo"}"#);

        let TileNode::Pane(p1) = &split.children[1] else {
            panic!("expected pane");
        };
        assert_eq!(p1.active_index, 1);
        assert!(matches!(p1.tab_orientation, TabOrientation::Vertical));
        assert!(p1.hide_tab_bar);
        assert_eq!(p1.locked_size, Some((300.0, 200.0)));
        assert_eq!(p1.items.len(), 2);
    }

    /// Pins the wire format facet-json produced before the serde migration:
    /// externally tagged enum variants, `null` options, and additive fields
    /// defaulting when absent.
    #[test]
    fn reads_pre_serde_layout_json() {
        let json = r#"{"version":1,"root":{"Split":{"axis":"Vertical","flexes":[1,1],"children":[{"Pane":{"active_index":0,"tab_orientation":"Horizontal","hide_tab_bar":false,"locked_size":null,"items":[{"kind":"node_editor","state":"{}"}]}},{"Pane":{"active_index":0,"tab_orientation":"Vertical","hide_tab_bar":true,"locked_size":[300.0,200.0],"items":[]}}]}}}"#;
        let parsed: TileLayout = serde_json::from_str(json).expect("legacy layout parses");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.global_time_range, "");
        let TileNode::Split(split) = parsed.root else {
            panic!("expected split");
        };
        assert!(matches!(split.axis, SplitAxis::Vertical));
        let TileNode::Pane(p1) = &split.children[1] else {
            panic!("expected pane");
        };
        assert_eq!(p1.locked_size, Some((300.0, 200.0)));
    }
}
