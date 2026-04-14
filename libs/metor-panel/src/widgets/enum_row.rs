use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::icons::Icon;
use crate::theme::theme;

/// Enum picker row. Displays the current selection and cascades to an
/// option list on activation.
pub struct EnumRow {
    pub label: SharedString,
    pub selected: SharedString,
    pub options: Vec<SharedString>,
    pub on_select: Arc<dyn Fn(String, &mut Window, &mut App)>,
}

impl InspectorRow for EnumRow {
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
                            .child(self.selected.clone()),
                    )
                    .child(Icon::ChevronRight.svg(8.0)),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let on_select = self.on_select.clone();
        let options = self.options.clone();

        let rows: Vec<Box<dyn InspectorRow>> = options
            .into_iter()
            .map(|opt| {
                let on_select = on_select.clone();
                let opt_str = opt.to_string();
                Box::new(EnumOptionRow {
                    label: opt,
                    on_select,
                    value: opt_str,
                }) as Box<dyn InspectorRow>
            })
            .collect();

        RowAction::Cascade(rows)
    }
}

/// A single option in an enum picker cascade.
struct EnumOptionRow {
    label: SharedString,
    value: String,
    on_select: Arc<dyn Fn(String, &mut Window, &mut App)>,
}

impl InspectorRow for EnumOptionRow {
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

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        (self.on_select)(self.value.clone(), window, cx);
        RowAction::Dismiss
    }
}
