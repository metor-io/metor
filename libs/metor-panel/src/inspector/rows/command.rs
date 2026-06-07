use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window};

use super::{InspectorRow, RowAction, render_label_row};
use crate::theme::theme;

/// Leaf row that runs a callback when activated.
///
/// Two flavors share one type because they render identically and differ
/// only in what happens after the callback:
/// - [`CommandRow::new`] takes a plain `Fn` and dismisses the inspector —
///   the common case (e.g. "Reset Camera", "Add Model").
/// - [`CommandRow::action`] takes a callback that returns its own
///   [`RowAction`], for rows that drill into a sub-page or stay open.
pub struct CommandRow {
    pub label: SharedString,
    callback: Arc<dyn Fn(&mut Window, &mut App) -> RowAction>,
    tag: Option<SharedString>,
}

impl CommandRow {
    pub fn new(
        label: impl Into<SharedString>,
        callback: Arc<dyn Fn(&mut Window, &mut App)>,
    ) -> Self {
        Self {
            label: label.into(),
            callback: Arc::new(move |window, cx| {
                callback(window, cx);
                RowAction::Dismiss
            }),
            tag: None,
        }
    }

    pub fn action(
        label: impl Into<SharedString>,
        callback: Arc<dyn Fn(&mut Window, &mut App) -> RowAction>,
    ) -> Self {
        Self {
            label: label.into(),
            callback,
            tag: None,
        }
    }

    pub fn with_tag(mut self, tag: impl Into<SharedString>) -> Self {
        self.tag = Some(tag.into());
        self
    }
}

impl InspectorRow for CommandRow {
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
        let color = theme(cx).text_primary;
        render_label_row(
            row_ix,
            selected,
            self.label.clone(),
            self.tag.clone(),
            color,
            cx,
        )
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        (self.callback)(window, cx)
    }
}
