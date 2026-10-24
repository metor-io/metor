use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, ROW_HEIGHT, RowAction};
use crate::theme::theme;

/// Non-interactive section label separating groups of rows.
///
/// Rendered muted and one step smaller than a normal row, without the hover
/// or selection highlight of [`row_base`](super::row_base). [`is_header`]
/// keeps it off the keyboard-selection path and out of filtered results, so
/// it never leaves an empty header behind once the user starts typing.
///
/// [`is_header`]: InspectorRow::is_header
pub struct HeaderRow {
    text: SharedString,
}

impl HeaderRow {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }
}

impl InspectorRow for HeaderRow {
    fn label(&self) -> &str {
        &self.text
    }

    fn render_row(
        &self,
        _row_ix: usize,
        _selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        // `row_base`'s height keeps items uniform in the inspector's
        // `uniform_list`, minus the interactive chrome.
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(ROW_HEIGHT))
            .px(px(12.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_tertiary)
                    .child(self.text.clone()),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        RowAction::Handled
    }

    fn is_header(&self) -> bool {
        true
    }
}
