use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{CommandRow, InspectorRow, RowAction, row_base, row_label, row_value};
use crate::icons::Icon;
use crate::theme::theme;

/// Inspector row for an enum field.
///
/// The summary shows the current variant; activation cascades into a list
/// with one [`CommandRow`] per declared variant.
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
            .gap(px(6.0))
            .child(row_label(self.label.clone(), theme.text_primary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .justify_end()
                    .child(row_value(self.selected.clone(), theme.text_secondary))
                    .child(Icon::ChevronRight.svg(8.0, theme.text_secondary)),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let rows: Vec<Box<dyn InspectorRow>> = self
            .options
            .iter()
            .map(|opt| {
                let on_select = self.on_select.clone();
                let value = opt.to_string();
                Box::new(CommandRow::new(
                    opt.clone(),
                    Arc::new(move |window, cx| on_select(value.clone(), window, cx)),
                )) as Box<dyn InspectorRow>
            })
            .collect();

        RowAction::Cascade(rows)
    }
}
