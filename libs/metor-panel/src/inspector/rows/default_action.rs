use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window};

use super::{InspectorRow, RowAction, render_label_row};
use crate::theme::theme;

/// Prompt row whose input is the inspector's search field itself.
///
/// Used for "type a value and press enter" flows (renaming a dashboard,
/// entering a new component value, adding a filter pattern). The
/// inspector forwards its current query string on activation, so the
/// user types once instead of clicking through a second inline editor.
///
/// [`DefaultActionRow::new`] treats the input as required: an empty search
/// falls back to [`RowAction::StartEdit`] so a bare Enter still opens the
/// inline editor. [`DefaultActionRow::optional`] is for rows that mean
/// something on their own (a shelf duration with an optional reason) — an
/// empty search runs the callback with an empty string, so a click does what
/// it says instead of opening an editor the user didn't ask for.
pub struct DefaultActionRow {
    label: SharedString,
    callback: Arc<dyn Fn(String, &mut Window, &mut App)>,
    require_input: bool,
}

impl DefaultActionRow {
    pub fn new(
        label: impl Into<SharedString>,
        callback: Arc<dyn Fn(String, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            label: label.into(),
            callback,
            require_input: true,
        }
    }

    pub fn optional(
        label: impl Into<SharedString>,
        callback: Arc<dyn Fn(String, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            label: label.into(),
            callback,
            require_input: false,
        }
    }
}

impl InspectorRow for DefaultActionRow {
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
        let color = theme(cx).text_tertiary;
        render_label_row(row_ix, selected, self.label.clone(), None, color, cx)
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        if !self.require_input {
            (self.callback)(String::new(), window, cx);
            return RowAction::Dismiss;
        }
        let cb = self.callback.clone();
        RowAction::StartEdit {
            current_text: String::new(),
            on_commit: Box::new(move |text, w, cx| {
                cb(text, w, cx);
            }),
        }
    }

    fn activate_with_search(
        &mut self,
        search: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> RowAction {
        if search.is_empty() {
            return self.activate(window, cx);
        }
        (self.callback)(search.to_string(), window, cx);
        RowAction::Dismiss
    }

    fn consumes_search(&self) -> bool {
        true
    }
}
