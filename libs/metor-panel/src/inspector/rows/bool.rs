use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, checkbox, row_base};
use crate::theme::theme;

/// Toggle switch row for boolean fields.
pub struct BoolRow {
    pub label: SharedString,
    pub value: bool,
    pub toggle: Arc<dyn Fn(bool, &mut Window, &mut App)>,
}

impl InspectorRow for BoolRow {
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
            .child(checkbox(self.value, &theme))
            .into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        self.value = !self.value;
        (self.toggle)(self.value, window, cx);
        RowAction::Handled
    }
}
