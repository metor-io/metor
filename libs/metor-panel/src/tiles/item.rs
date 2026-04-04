use gpui::{AnyView, App, Entity, Render, SharedString};

/// Trait that pane content must implement to be hosted in a tile pane.
pub trait PaneItem: Render + Sized + 'static {
    /// Display title shown in the tab bar.
    fn tab_title(&self, cx: &App) -> SharedString;

    /// Unique type key used for serialization dispatch (e.g. "time_series_plot").
    fn serialization_key() -> &'static str;

    /// Serialize this item's state to a JSON value for persistence.
    fn serialize(&self, cx: &App) -> serde_json::Value;

    /// Whether this tab can be closed by the user. Default: true.
    fn can_close(&self, _cx: &App) -> bool {
        true
    }
}

/// Object-safe handle to any pane item, used for type-erased storage in Pane.
pub trait PaneItemHandle: 'static {
    fn tab_title(&self, cx: &App) -> SharedString;
    fn serialization_key(&self) -> &'static str;
    fn serialize(&self, cx: &App) -> serde_json::Value;
    fn can_close(&self, cx: &App) -> bool;
    fn view(&self) -> AnyView;
    fn entity_id(&self) -> gpui::EntityId;
    fn clone_handle(&self) -> Box<dyn PaneItemHandle>;
}

impl<T: PaneItem> PaneItemHandle for Entity<T> {
    fn tab_title(&self, cx: &App) -> SharedString {
        self.read(cx).tab_title(cx)
    }

    fn serialization_key(&self) -> &'static str {
        T::serialization_key()
    }

    fn serialize(&self, cx: &App) -> serde_json::Value {
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
}
