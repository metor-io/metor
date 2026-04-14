use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

/// One-shot action row (e.g., "Reset Camera", "Add Model").
pub struct CommandRow {
    pub label: SharedString,
    pub callback: Arc<dyn Fn(&mut Window, &mut App)>,
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
        let theme = theme(cx);
        row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(self.label.clone()),
            )
            .into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        (self.callback)(window, cx);
        RowAction::Dismiss
    }
}
