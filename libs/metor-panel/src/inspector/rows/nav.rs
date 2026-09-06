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
    identity: Option<SharedString>,
    summary: Box<dyn Fn(&App) -> SharedString>,
    pub build_children: Box<dyn Fn(&gpui::App) -> Vec<Box<dyn InspectorRow>>>,
    tag: Option<SharedString>,
    accessory: Option<Box<dyn Fn(&mut App) -> Option<super::AccessorySpec>>>,
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
            identity: None,
            summary: Box::new(move |_| summary.clone()),
            build_children,
            tag: None,
            accessory: None,
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
            identity: None,
            build_children,
            tag: None,
            accessory: None,
            query: None,
        }
    }

    /// Attach a page companion while keeping navigation in the shared row model.
    pub fn with_accessory(
        mut self,
        build: Box<dyn Fn(&mut App) -> Option<super::AccessorySpec>>,
    ) -> Self {
        self.accessory = Some(build);
        self
    }

    pub fn with_tag(mut self, tag: impl Into<SharedString>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    pub(crate) fn with_identity(mut self, identity: SharedString) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Open the child page with `query` already in its search field.
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }
}

impl InspectorRow for NavRow {
    fn identity(&self) -> SharedString {
        self.identity.clone().unwrap_or_else(|| self.label.clone())
    }
    fn supports_exit_fade(&self) -> bool {
        true
    }

    fn accessory(&self, _: &str, cx: &mut App) -> Option<super::AccessorySpec> {
        self.accessory.as_ref().and_then(|build| build(cx))
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);

        let summary = (self.summary)(cx);
        // What the right side takes is what the label cannot have: the
        // summary up to its cap, the tag, and the chevron.
        let budget = super::label_budget(cx).map(|width| {
            let summary_w = super::measure(&summary, px(12.0), window).min(px(260.0));
            let tag_w = self
                .tag
                .as_ref()
                .map(|tag| super::measure(tag, px(10.0), window) + px(18.0))
                .unwrap_or_default();
            width - summary_w - tag_w - px(26.0)
        });
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
            .child(super::path_label(
                &self.label,
                theme.text_primary,
                budget,
                window,
                cx,
            ))
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
