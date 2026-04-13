use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
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
        let val = self.value;
        let track_color = if val {
            theme.line_color
        } else {
            theme.text_tertiary
        };

        let knob = div()
            .w(px(10.0))
            .h(px(10.0))
            .rounded(px(2.0))
            .bg(theme.text_primary);

        let track = div()
            .w(px(28.0))
            .h(px(14.0))
            .rounded(px(3.0))
            .bg(track_color)
            .flex()
            .items_center()
            .px(px(2.0))
            .when(val, |el| el.flex_row_reverse())
            .child(knob);

        row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(self.label.clone()),
            )
            .child(track)
            .into_any_element()
    }

    fn activate(&self, window: &mut Window, cx: &mut App) -> RowAction {
        (self.toggle)(!self.value, window, cx);
        RowAction::Handled
    }
}
