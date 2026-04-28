use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

/// Inspector row for a string field.
///
/// Activation drops into inline editing; on commit the row's cached value
/// is updated immediately so the summary redraws without waiting for the
/// next read from the underlying entity.
pub struct TextRow {
    pub label: SharedString,
    pub value: Rc<RefCell<SharedString>>,
    pub on_change: Arc<dyn Fn(String, &mut Window, &mut App)>,
}

impl TextRow {
    pub fn new(
        label: SharedString,
        value: SharedString,
        on_change: Arc<dyn Fn(String, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            label,
            value: Rc::new(RefCell::new(value)),
            on_change,
        }
    }
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
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .child(self.value.borrow().clone()),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let on_change = self.on_change.clone();
        let value = self.value.clone();
        RowAction::StartEdit {
            current_text: self.value.borrow().to_string(),
            on_commit: Box::new(move |text, window, cx| {
                *value.borrow_mut() = SharedString::new(text.clone());
                on_change(text, window, cx);
            }),
        }
    }
}
