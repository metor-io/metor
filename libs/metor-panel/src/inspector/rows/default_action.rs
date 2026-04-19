use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

/// A row that prompts for inline text input, then forwards the committed
/// text to a callback.
///
/// Used to model "type a value and press enter" prompts (e.g. renaming a
/// dashboard, typing a new component value) inside the unified inspector.
pub struct DefaultActionRow {
    pub label: SharedString,
    pub callback: Arc<dyn Fn(String, &mut Window, &mut App)>,
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
        let theme = theme(cx);
        row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(self.label.clone()),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let cb = self.callback.clone();
        RowAction::StartEdit {
            current_text: String::new(),
            on_commit: Box::new(move |text, w, cx| {
                cb(text, w, cx);
            }),
        }
    }
}
