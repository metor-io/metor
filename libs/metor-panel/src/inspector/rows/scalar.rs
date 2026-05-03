use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

/// Inspector row for a numeric field.
///
/// Shows the current value as text; activation starts inline editing.
/// Unparsable input is silently dropped rather than raising an error.
///
/// The current value is cached in an `Rc<RefCell<f64>>` so that on commit
/// the row redraws with the new value immediately, instead of waiting for
/// the underlying entity to refresh.
pub struct ScalarRow {
    pub label: SharedString,
    pub value: Rc<RefCell<f64>>,
    pub on_change: Arc<dyn Fn(f64, &mut Window, &mut App)>,
}

impl ScalarRow {
    pub fn new(
        label: SharedString,
        value: f64,
        on_change: Arc<dyn Fn(f64, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            label,
            value: Rc::new(RefCell::new(value)),
            on_change,
        }
    }
}

impl InspectorRow for ScalarRow {
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
        let value_text = SharedString::from(format!("{}", *self.value.borrow()));

        row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(self.label.clone()),
            )
            .child(
                div().min_w(px(60.0)).max_w(px(120.0)).child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_secondary)
                        .child(value_text),
                ),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let on_change = self.on_change.clone();
        let cached = self.value.clone();
        RowAction::StartEdit {
            current_text: format!("{}", *self.value.borrow()),
            on_commit: Box::new(move |text, window, cx| {
                if let Ok(v) = text.parse::<f64>() {
                    *cached.borrow_mut() = v;
                    on_change(v, window, cx);
                }
            }),
        }
    }
}
