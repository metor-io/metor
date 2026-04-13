use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::icons::Icon;
use crate::theme::theme;

/// A navigable row that cascades to sub-rows built by a factory closure.
///
/// The factory is called on each activation, producing fresh rows that
/// reflect the current state. This is the building block for lists,
/// nested structs, and any row that opens a child panel.
pub struct NavRow {
    pub label: SharedString,
    pub summary: SharedString,
    pub build_children: Arc<dyn Fn(&gpui::App) -> Vec<Box<dyn InspectorRow>>>,
}

impl InspectorRow for NavRow {
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
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_secondary)
                            .child(self.summary.clone()),
                    )
                    .child(Icon::ChevronRight.svg(8.0)),
            )
            .into_any_element()
    }

    fn activate(&self, _window: &mut Window, cx: &mut App) -> RowAction {
        RowAction::Cascade((self.build_children)(cx))
    }
}
