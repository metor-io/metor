use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, checkbox, row_base};
use crate::theme::theme;

/// Inspector row for a `bool` field, rendered as a checkbox.
///
/// The default form owns its bool and flips it locally on activation,
/// mirroring the value into the entity through `toggle`. The
/// [`BoolRow::dynamic`] form instead reads the checked state from a
/// closure each render, so the row can reflect external state that
/// changes outside its own activation (e.g. a "select all" affecting
/// sibling rows).
pub struct BoolRow {
    pub label: SharedString,
    pub value: bool,
    value_source: Option<Box<dyn Fn(&App) -> bool>>,
    pub toggle: Arc<dyn Fn(bool, &mut Window, &mut App)>,
}

impl BoolRow {
    pub fn new(
        label: impl Into<SharedString>,
        value: bool,
        toggle: Arc<dyn Fn(bool, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            label: label.into(),
            value,
            value_source: None,
            toggle,
        }
    }

    pub fn dynamic(
        label: impl Into<SharedString>,
        value: Box<dyn Fn(&App) -> bool>,
        toggle: Arc<dyn Fn(bool, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            label: label.into(),
            value: false,
            value_source: Some(value),
            toggle,
        }
    }

    fn current(&self, cx: &App) -> bool {
        match &self.value_source {
            Some(f) => f(cx),
            None => self.value,
        }
    }
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
            .child(checkbox(self.current(cx), &theme))
            .into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        let next = !self.current(cx);
        if self.value_source.is_none() {
            self.value = next;
        }
        (self.toggle)(next, window, cx);
        RowAction::Handled
    }
}
