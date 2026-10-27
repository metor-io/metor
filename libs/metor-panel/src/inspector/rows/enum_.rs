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
    source: Option<Arc<dyn Fn(&App) -> Option<SharedString>>>,
    pub options: Vec<SharedString>,
    pub on_select: Arc<dyn Fn(String, &mut Window, &mut App)>,
}

impl EnumRow {
    pub fn new(
        label: impl Into<SharedString>,
        selected: impl Into<SharedString>,
        options: Vec<SharedString>,
        on_select: Arc<dyn Fn(String, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            label: label.into(),
            selected: selected.into(),
            source: None,
            options,
            on_select,
        }
    }

    /// Read the value through `source` on every render, falling back to the
    /// cached copy when it answers `None`. A docked inspector outlives the
    /// edit that built it, so a row there has to track changes made
    /// elsewhere — by the popup, or by the view itself.
    pub fn live(mut self, source: Arc<dyn Fn(&App) -> Option<SharedString>>) -> Self {
        self.source = Some(source);
        self
    }

    fn current(&self, cx: &App) -> SharedString {
        self.source
            .as_ref()
            .and_then(|source| source(cx))
            .unwrap_or_else(|| self.selected.clone())
    }
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
                    .child(row_value(self.current(cx), theme.text_secondary))
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
