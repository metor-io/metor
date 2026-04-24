use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

/// Leaf row whose callback chooses its own follow-up [`RowAction`].
///
/// Unlike [`CommandRow`](super::CommandRow) which always dismisses, this row
/// returns whatever the callback produces — useful when a row needs to stay
/// in the inspector (e.g., refresh the current page) rather than close it.
pub struct ActionRow {
    pub label: SharedString,
    pub callback: Arc<dyn Fn(&mut Window, &mut App) -> RowAction>,
    tag: Option<SharedString>,
    consumes_search: bool,
}

impl ActionRow {
    pub fn new(
        label: impl Into<SharedString>,
        callback: Arc<dyn Fn(&mut Window, &mut App) -> RowAction>,
    ) -> Self {
        Self {
            label: label.into(),
            callback,
            tag: None,
            consumes_search: false,
        }
    }

    pub fn with_tag(mut self, tag: impl Into<SharedString>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    pub fn pinned(mut self) -> Self {
        self.consumes_search = true;
        self
    }
}

impl InspectorRow for ActionRow {
    fn label(&self) -> &str {
        &self.label
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        let mut row = row_base(row_ix, selected, cx).child(
            div()
                .text_size(px(12.0))
                .text_color(theme.text_primary)
                .child(self.label.clone()),
        );
        if let Some(tag) = &self.tag {
            row = row.child(super::tag_pill(tag.clone(), cx));
        }
        row.into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        (self.callback)(window, cx)
    }

    fn consumes_search(&self) -> bool {
        self.consumes_search
    }
}
