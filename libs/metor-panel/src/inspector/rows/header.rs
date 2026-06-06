use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

/// Non-interactive section label at the top of an inspector page.
///
/// Rendered muted and one step smaller than a normal row. Keeps the default
/// `consumes_search: false` so it filters out as soon as the user types — a
/// pinned header that jumped to the bottom of the results (where the
/// inspector parks search-consuming commit rows) would be more confusing
/// than one that simply disappears.
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
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_secondary)
                    .child(self.text.clone()),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        RowAction::Handled
    }
}
