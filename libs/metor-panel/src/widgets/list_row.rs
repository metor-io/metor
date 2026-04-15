use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};

use super::{InspectorRow, RowAction, row_base};
use crate::icons::Icon;
use crate::theme::theme;

/// A navigable row that cascades to sub-rows built by a factory closure.
///
/// The factory is called on each activation, producing fresh rows that
/// reflect the current state. This is the building block for lists,
/// nested structs, and any row that opens a child panel.
pub struct NavRow {
    pub label: SharedString,
    pub summary: SharedString,
    pub build_children: Box<dyn Fn(&gpui::App) -> Vec<Box<dyn InspectorRow>>>,
    /// Optional category pill rendered alongside the summary.
    tag: Option<SharedString>,
}

impl NavRow {
    pub fn new(
        label: impl Into<SharedString>,
        summary: impl Into<SharedString>,
        build_children: Box<dyn Fn(&gpui::App) -> Vec<Box<dyn InspectorRow>>>,
    ) -> Self {
        Self {
            label: label.into(),
            summary: summary.into(),
            build_children,
            tag: None,
        }
    }

    pub fn with_tag(mut self, tag: impl Into<SharedString>) -> Self {
        self.tag = Some(tag.into());
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

        let mut right = div().flex().flex_row().items_center().gap(px(6.0));
        if let Some(tag) = &self.tag {
            right = right.child(super::tag_pill(tag.clone(), cx));
        }
        right = right
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(self.summary.clone()),
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
        RowAction::Cascade((self.build_children)(cx))
    }
}
