use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::icons::Icon;
use crate::theme::theme;

/// Drill-down row whose child page is rebuilt from scratch on every
/// activation.
///
/// Re-evaluating the factory each time keeps cascaded pages in sync with
/// the world without listener bookkeeping: the child rows see the state
/// live on entry rather than a stale snapshot.
///
/// The right-hand summary is read through a closure each render, so it
/// stays in sync with shared state mutated from elsewhere (a sub-page
/// committing a selection, for example). Use [`NavRow::new`] for a fixed
/// summary or [`NavRow::with_dynamic_summary`] for a live one.
pub struct NavRow {
    pub label: SharedString,
    summary: Box<dyn Fn(&App) -> SharedString>,
    pub build_children: Box<dyn Fn(&gpui::App) -> Vec<Box<dyn InspectorRow>>>,
    tag: Option<SharedString>,
    /// Text the child page opens with in its search field, when the page
    /// edits something that already has one.
    query: Option<String>,
}

impl NavRow {
    pub fn new(
        label: impl Into<SharedString>,
        summary: impl Into<SharedString>,
        build_children: Box<dyn Fn(&gpui::App) -> Vec<Box<dyn InspectorRow>>>,
    ) -> Self {
        let summary = summary.into();
        Self {
            label: label.into(),
            summary: Box::new(move |_| summary.clone()),
            build_children,
            tag: None,
            query: None,
        }
    }

    pub fn with_dynamic_summary(
        label: impl Into<SharedString>,
        summary: Box<dyn Fn(&App) -> SharedString>,
        build_children: Box<dyn Fn(&gpui::App) -> Vec<Box<dyn InspectorRow>>>,
    ) -> Self {
        Self {
            label: label.into(),
            summary,
            build_children,
            tag: None,
            query: None,
        }
    }

    pub fn with_tag(mut self, tag: impl Into<SharedString>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Open the child page with `query` already in its search field.
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }
}

impl InspectorRow for NavRow {
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

        let summary = (self.summary)(cx);
        let mut right = div().flex().flex_row().items_center().gap(px(6.0));
        if let Some(tag) = &self.tag {
            right = right.child(super::tag_pill(tag.clone(), cx));
        }
        right = right
            .child(
                // Cap the summary so long descriptions (e.g. a comma-joined
                // component list) ellipsize instead of spilling past the
                // row's rounded highlight.
                div()
                    .max_w(px(260.0))
                    .truncate()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(summary),
            )
            .child(Icon::ChevronRight.svg(8.0));

        row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(self.label.clone()),
            )
            .child(right)
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, cx: &mut App) -> RowAction {
        let rows = (self.build_children)(cx);
        match &self.query {
            Some(query) => RowAction::CascadeWith {
                rows,
                query: query.clone(),
            },
            None => RowAction::Cascade(rows),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row that edits an existing binding opens its page on that binding.
    #[gpui::test]
    fn a_query_rides_along_into_the_child_page(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            let mut plain = NavRow::new("Source", "", Box::new(|_cx| Vec::new()));
            assert!(matches!(plain.activate(window, cx), RowAction::Cascade(_)));

            let mut seeded = NavRow::new("Source", "", Box::new(|_cx| Vec::new()))
                .with_query("=adcs.omega_b @ adcs.omega_b");
            let RowAction::CascadeWith { query, .. } = seeded.activate(window, cx) else {
                panic!("a seeded row opens its page on the query");
            };
            assert_eq!(query, "=adcs.omega_b @ adcs.omega_b");
        });
    }
}
