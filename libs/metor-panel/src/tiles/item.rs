use gpui::{AnyEntity, AnyView, App, Entity, Render, SharedString};

/// Content hosted inside a pane as a single tab.
///
/// Implementers gain a tab title, JSON persistence, and optional inspection
/// support. The trait is generic so concrete panel types can keep their own
/// APIs; [`PaneItemHandle`] erases them for storage.
pub trait PaneItem: Render + Sized + 'static {
    fn tab_title(&self, cx: &App) -> SharedString;

    /// Stable identifier written into the serialized layout. Changing it
    /// breaks compatibility with previously-saved workspaces.
    fn serialization_key() -> &'static str;

    /// Snapshot the item's own state as a facet-json blob. Paired with an
    /// [`ItemRegistry`] deserializer keyed on [`PaneItem::serialization_key`];
    /// the registered closure receives the same string and is responsible for
    /// parsing it (typically with `facet_json::from_str` into a `*Config`).
    fn serialize(&self, cx: &App) -> String;

    /// Whether the tab bar should render a close affordance for this item.
    fn can_close(&self, _cx: &App) -> bool {
        true
    }

    /// Optional override for reflection-based inspection. Wrapper panels
    /// return their inner entity so the inspector walks the real data model
    /// rather than the wrapper.
    fn inspectable_entity(&self) -> Option<AnyEntity> {
        None
    }
}

/// Object-safe view of a [`PaneItem`] so heterogeneous panels can share a tab bar.
pub trait PaneItemHandle: 'static {
    fn tab_title(&self, cx: &App) -> SharedString;
    fn serialization_key(&self) -> &'static str;
    fn serialize(&self, cx: &App) -> String;
    fn can_close(&self, cx: &App) -> bool;
    fn view(&self) -> AnyView;
    fn entity_id(&self) -> gpui::EntityId;
    fn clone_handle(&self) -> Box<dyn PaneItemHandle>;

    /// Resolve the entity used by the reflection-driven inspector, applying
    /// [`PaneItem::inspectable_entity`] when set.
    fn entity_any(&self, cx: &App) -> AnyEntity;
}

impl<T: PaneItem> PaneItemHandle for Entity<T> {
    fn tab_title(&self, cx: &App) -> SharedString {
        self.read(cx).tab_title(cx)
    }

    fn serialization_key(&self) -> &'static str {
        T::serialization_key()
    }

    fn serialize(&self, cx: &App) -> String {
        self.read(cx).serialize(cx)
    }

    fn can_close(&self, cx: &App) -> bool {
        self.read(cx).can_close(cx)
    }

    fn view(&self) -> AnyView {
        AnyView::from(self.clone())
    }

    fn entity_id(&self) -> gpui::EntityId {
        Entity::entity_id(self)
    }

    fn clone_handle(&self) -> Box<dyn PaneItemHandle> {
        Box::new(self.clone())
    }

    fn entity_any(&self, cx: &App) -> AnyEntity {
        self.read(cx)
            .inspectable_entity()
            .unwrap_or_else(|| self.clone().into_any())
    }
}
