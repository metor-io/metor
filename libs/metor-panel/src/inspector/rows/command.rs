use std::sync::Arc;

use gpui::{AnyElement, App, Hsla, SharedString, Window, div, prelude::*, px};
use smallvec::SmallVec;

use super::{InspectorRow, RowAction, render_label_row};
use crate::theme::theme;

/// Leaf row that runs a callback when activated.
///
/// Two flavors share one type because they render identically and differ
/// only in what happens after the callback:
/// - [`CommandRow::new`] takes a plain `Fn` and dismisses the inspector —
///   the common case (e.g. "Reset Camera", "Add Model").
/// - [`CommandRow::action`] takes a callback that returns its own
///   [`RowAction`], for rows that drill into a sub-page or stay open.
pub struct CommandRow {
    pub label: SharedString,
    callback: Arc<dyn Fn(&mut Window, &mut App) -> RowAction>,
    tag: Option<SharedString>,
    /// A tag read on every render, for one that follows shared state.
    live_tag: Option<Arc<dyn Fn(&App) -> Option<SharedString>>>,
    consumes_search: bool,
    /// Colours shown after the label, for a row that picks a look.
    swatches: SmallVec<[Hsla; 4]>,
}

impl CommandRow {
    pub fn new(
        label: impl Into<SharedString>,
        callback: Arc<dyn Fn(&mut Window, &mut App)>,
    ) -> Self {
        Self {
            label: label.into(),
            callback: Arc::new(move |window, cx| {
                callback(window, cx);
                RowAction::Dismiss
            }),
            tag: None,
            live_tag: None,
            consumes_search: false,
            swatches: SmallVec::new(),
        }
    }

    pub fn action(
        label: impl Into<SharedString>,
        callback: Arc<dyn Fn(&mut Window, &mut App) -> RowAction>,
    ) -> Self {
        Self {
            label: label.into(),
            callback,
            tag: None,
            live_tag: None,
            consumes_search: false,
            swatches: SmallVec::new(),
        }
    }

    /// Keep the row visible whatever the search says, for pinned
    /// affordances like a page's "Done".
    pub fn pinned(mut self) -> Self {
        self.consumes_search = true;
        self
    }

    pub fn with_tag(mut self, tag: impl Into<SharedString>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Show the tag `tag` answers, read on every render.
    pub fn with_live_tag(mut self, tag: Arc<dyn Fn(&App) -> Option<SharedString>>) -> Self {
        self.live_tag = Some(tag);
        self
    }

    /// Show `colors` as a strip after the label: a palette, say.
    pub fn with_swatches(mut self, colors: impl IntoIterator<Item = Hsla>) -> Self {
        self.swatches = colors.into_iter().collect();
        self
    }
}

impl InspectorRow for CommandRow {
    fn identity(&self) -> SharedString {
        format!("command:{:?}:{}", self.tag, self.label).into()
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn consumes_search(&self) -> bool {
        self.consumes_search
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let color = theme(cx).text_primary;
        let tag = match &self.live_tag {
            Some(live) => live(cx),
            None => self.tag.clone(),
        };
        let row = render_label_row(row_ix, selected, self.label.clone(), tag, color, window, cx);
        if self.swatches.is_empty() {
            return row;
        }
        let strip = self.swatches.iter().fold(
            div().flex().flex_row().flex_none().ml_auto(),
            |strip, color| strip.child(div().w(px(12.0)).h(px(12.0)).bg(*color)),
        );
        div()
            .flex()
            .flex_row()
            .items_center()
            .child(div().flex_1().min_w_0().child(row))
            .child(strip.mr(px(10.0)).rounded(px(3.0)).overflow_hidden())
            .into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        (self.callback)(window, cx)
    }
}
