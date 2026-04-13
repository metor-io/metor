use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

/// Text-editable string field row.
pub struct TextRow {
    pub label: SharedString,
    pub value: SharedString,
    pub on_change: Arc<dyn Fn(String, &mut Window, &mut App)>,
}

impl InspectorRow for TextRow {
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
            .child(
                div()
                    .min_w(px(60.0))
                    .max_w(px(120.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_secondary)
                            .child(self.value.clone()),
                    ),
            )
            .into_any_element()
    }

    fn activate(&self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let on_change = self.on_change.clone();
        RowAction::StartEdit {
            current_text: self.value.to_string(),
            on_commit: Box::new(move |text, window, cx| {
                on_change(text, window, cx);
            }),
        }
    }
}
