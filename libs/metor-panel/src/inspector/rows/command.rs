use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::theme::theme;

/// Row that runs a callback once and dismisses the inspector.
///
/// Used for leaf actions like "Reset Camera" or "Add Model" where there's
/// no follow-up page.
pub struct CommandRow {
    pub label: SharedString,
    pub callback: Arc<dyn Fn(&mut Window, &mut App)>,
    tag: Option<SharedString>,
}

impl CommandRow {
    pub fn new(
        label: impl Into<SharedString>,
        callback: Arc<dyn Fn(&mut Window, &mut App)>,
    ) -> Self {
        Self {
            label: label.into(),
            callback,
            tag: None,
        }
    }

    pub fn with_tag(mut self, tag: impl Into<SharedString>) -> Self {
        self.tag = Some(tag.into());
        self
    }
}

impl InspectorRow for CommandRow {
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
        let mut row = row_base(row_ix, selected, cx).child(
            div()
                .text_size(px(12.0))
                .text_color(theme.text_primary)
                .child(self.label.clone()),
        );
        if let Some(tag) = &self.tag {
            row = row.child(super::tag_pill(tag.clone(), cx));
        }
        row.into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        (self.callback)(window, cx);
        RowAction::Dismiss
    }
}
