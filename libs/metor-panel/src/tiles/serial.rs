use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyEntity, AnyView, App, Context, Entity, Global, IntoElement, Render, SharedString, Window,
    div, prelude::*, px,
};
use metor_db::DB;

pub use metor_proto_wkt::{
    SplitAxis, TILE_LAYOUT_VERSION, TabOrientation, TileItem, TileLayout, TileNode, TilePane,
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
/// Missing registrations hydrate as inert placeholders that retain the raw
/// kind and state until the providing plugin is installed again.
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

    /// Register an erased pane constructor. This is the low-level extension
    /// point behind both typed [`PaneItem`] registrations and shared view
    /// registrations.
    pub fn register_erased(
        &mut self,
        key: impl Into<String>,
        deserialize: impl Fn(&str, &mut Context<super::pane::Pane>) -> Option<Box<dyn PaneItemHandle>>
        + 'static,
    ) {
        Arc::make_mut(&mut self.deserializers).insert(key.into(), Arc::new(deserialize));
    }

    /// Adapt a cross-host view spec into a tile deserializer. The view's
    /// builder and snapshot callback are exactly the ones Dashboard uses.
    pub fn register_view(&mut self, spec: Arc<crate::views::dashboard::WidgetSpec>, db: Arc<DB>) {
        let Some(tile) = spec.tile.clone() else {
            return;
        };
        let key = tile.serialization_key.to_string();
        self.register_erased(key.clone(), move |state, cx| {
            let live = (spec.build)(state, &db, cx);
            let view = cx.new({
                let child = live.view.clone();
                move |_| RegisteredPane { child }
            });
            Some(Box::new(RegisteredPaneHandle {
                view,
                key: key.clone(),
                inspect: live.inspect,
                state: live.state,
                config: state.to_string(),
                snapshot: spec.snapshot.clone(),
                tab_title: tile.tab_title.clone(),
            }))
        });
    }

    /// Invoke the registered constructor for `kind`. Missing registrations
    /// become inert panes that preserve the raw blob for a future reload.
    pub fn deserialize(
        &self,
        kind: &str,
        state: &str,
        cx: &mut Context<super::pane::Pane>,
    ) -> Option<Box<dyn PaneItemHandle>> {
        match self.deserializers.get(kind) {
            Some(f) => f(state, cx),
            None => {
                let kind = kind.to_string();
                let state = state.to_string();
                let entity = cx.new({
                    let kind = kind.clone();
                    move |_| UnknownPane { kind }
                });
                Some(Box::new(UnknownPaneHandle {
                    entity,
                    kind,
                    state,
                }))
            }
        }
    }
}

struct RegisteredPane {
    child: AnyView,
}

impl Render for RegisteredPane {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.child.clone()
    }
}

#[derive(Clone)]
struct RegisteredPaneHandle {
    view: Entity<RegisteredPane>,
    key: String,
    inspect: AnyEntity,
    state: AnyEntity,
    config: String,
    snapshot: Arc<dyn Fn(&AnyEntity, &str, &App) -> Option<String>>,
    tab_title: Arc<dyn Fn(&AnyEntity, &str, &App) -> SharedString>,
}

impl PaneItemHandle for RegisteredPaneHandle {
    fn tab_title(&self, cx: &App) -> SharedString {
        (self.tab_title)(&self.state, &self.config, cx)
    }

    fn serialization_key(&self) -> &str {
        &self.key
    }

    fn serialize(&self, cx: &App) -> String {
        (self.snapshot)(&self.state, &self.config, cx).unwrap_or_else(|| self.config.clone())
    }

    fn can_close(&self, _cx: &App) -> bool {
        true
    }

    fn view(&self) -> AnyView {
        AnyView::from(self.view.clone())
    }

    fn entity_id(&self) -> gpui::EntityId {
        self.view.entity_id()
    }

    fn clone_handle(&self) -> Box<dyn PaneItemHandle> {
        Box::new(self.clone())
    }

    fn entity_any(&self, _cx: &App) -> AnyEntity {
        self.inspect.clone()
    }
}

/// Inert stand-in for a pane whose downstream registration is unavailable.
/// It deliberately implements the erased handle directly because its
/// serialization key is runtime data rather than a static Rust type value.
struct UnknownPane {
    kind: String,
}

#[derive(Clone)]
struct UnknownPaneHandle {
    entity: Entity<UnknownPane>,
    kind: String,
    state: String,
}

impl Render for UnknownPane {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.0))
            .child(format!("Missing view: {}", self.kind))
    }
}

impl PaneItemHandle for UnknownPaneHandle {
    fn tab_title(&self, _cx: &App) -> SharedString {
        SharedString::from(format!("Missing: {}", self.kind))
    }

    fn serialization_key(&self) -> &str {
        &self.kind
    }

    fn serialize(&self, _cx: &App) -> String {
        self.state.clone()
    }

    fn can_close(&self, _cx: &App) -> bool {
        true
    }

    fn view(&self) -> AnyView {
        AnyView::from(self.entity.clone())
    }

    fn entity_id(&self) -> gpui::EntityId {
        self.entity.entity_id()
    }

    fn clone_handle(&self) -> Box<dyn PaneItemHandle> {
        Box::new(self.clone())
    }

    fn entity_any(&self, _cx: &App) -> AnyEntity {
        self.entity.clone().into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_layout() -> TileLayout {
        TileLayout {
            version: TILE_LAYOUT_VERSION,
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

        assert_eq!(parsed.version, TILE_LAYOUT_VERSION);
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
}
