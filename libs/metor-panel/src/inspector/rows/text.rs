use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{AnyElement, App, Hsla, SharedString, Window, prelude::*, px};

use super::{InspectorRow, RowAction, row_base, row_label, row_value, value_row};
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
    source: Option<Arc<dyn Fn(&App) -> Option<SharedString>>>,
    pub on_change: Arc<dyn Fn(String, &mut Window, &mut App)>,
    pub read_only: bool,
    tint: Option<Arc<dyn Fn(&App) -> Option<Hsla>>>,
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
            source: None,
            on_change,
            read_only: false,
            tint: None,
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
            .unwrap_or_else(|| self.value.borrow().clone())
    }

    /// Read-only variant that renders muted and rejects activation. The
    /// `on_change` is a no-op so callers don't need to wire anything.
    pub fn readonly(label: SharedString, value: SharedString) -> Self {
        Self {
            label,
            value: Rc::new(RefCell::new(value)),
            source: None,
            on_change: Arc::new(|_, _, _| {}),
            read_only: true,
            tint: None,
        }
    }

    /// Color the value through `tint` on every render, for a status whose
    /// value says whether something worked. `None` keeps the usual color.
    pub fn tinted(mut self, tint: Arc<dyn Fn(&App) -> Option<Hsla>>) -> Self {
        self.tint = Some(tint);
        self
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
        if let Some(color) = self.tint.as_ref().and_then(|tint| tint(cx)) {
            let theme = theme(cx);
            return row_base(row_ix, selected, cx)
                .gap(px(2.))
                .child(row_label(self.label.clone(), theme.text_secondary))
                .child(row_value(self.current(cx), color))
                .into_any_element();
        }
        value_row(
            row_ix,
            selected,
            self.label.clone(),
            self.current(cx),
            self.read_only,
            cx,
        )
        .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, cx: &mut App) -> RowAction {
        if self.read_only {
            return RowAction::Handled;
        }
        let on_change = self.on_change.clone();
        let value = self.value.clone();
        RowAction::StartEdit {
            current_text: self.current(cx).to_string(),
            on_commit: Box::new(move |text, window, cx| {
                *value.borrow_mut() = SharedString::new(text.clone());
                on_change(text, window, cx);
            }),
        }
    }
}
