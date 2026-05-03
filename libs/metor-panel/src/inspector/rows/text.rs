use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

/// Inspector row for a string field.
///
/// Activation drops into inline editing; on commit the row's cached value
/// is updated immediately so the summary redraws without waiting for the
/// next read from the underlying entity.
///
/// `read_only` rows render in muted text and ignore activation — used to
/// show status / metadata that the user shouldn't edit.
pub struct TextRow {
    pub label: SharedString,
    pub value: Rc<RefCell<SharedString>>,
    pub on_change: Arc<dyn Fn(String, &mut Window, &mut App)>,
    pub read_only: bool,
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
            read_only: false,
        }
    }

    /// Read-only variant that renders muted and rejects activation. The
    /// `on_change` is a no-op so callers don't need to wire anything.
    pub fn new_readonly(label: SharedString, value: SharedString) -> Self {
        Self {
            label,
            value: Rc::new(RefCell::new(value)),
            on_change: Arc::new(|_, _, _| {}),
            read_only: true,
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
        let label_color = if self.read_only {
            theme.text_tertiary
        } else {
            theme.text_primary
        };
        let value_color = if self.read_only {
            theme.text_tertiary
        } else {
            theme.text_secondary
        };

        row_base(row_ix, selected, cx)
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(label_color)
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.0))
                    .text_color(value_color)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .child(self.value.borrow().clone()),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        if self.read_only {
            return RowAction::Handled;
        }
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
