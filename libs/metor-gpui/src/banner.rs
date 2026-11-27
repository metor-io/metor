//! An inline status strip across the top of a page, such as "reconnecting…".

use gpui::{App, IntoElement, RenderOnce, SharedString, Window, div, prelude::*, px};

use crate::{CONTROL_HEIGHT, StateTone, Theme, theme};

/// A one-line status strip with a state-colored edge, the message, and an optional trailing
/// element such as a retry button.
///
/// It sits in the layout flow rather than floating, so it pushes the page down while shown.
#[derive(IntoElement)]
pub struct Banner {
    tone: StateTone,
    message: SharedString,
    end: Option<gpui::AnyElement>,
}

impl Banner {
    /// A banner saying `message`, tinted by `tone`.
    pub fn new(tone: StateTone, message: impl Into<SharedString>) -> Self {
        Self {
            tone,
            message: message.into(),
            end: None,
        }
    }

    /// An element after the message, at the trailing edge.
    pub fn end(mut self, element: impl IntoElement) -> Self {
        self.end = Some(element.into_any_element());
        self
    }
}

impl RenderOnce for Banner {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let accent = theme.state_color(self.tone);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .w_full()
            .h(px(CONTROL_HEIGHT))
            .px(px(12.0))
            .bg(Theme::dim(accent, 0.12))
            .border_b_1()
            .border_color(Theme::dim(accent, 0.4))
            .text_size(px(12.0))
            .text_color(theme.text_primary)
            .child(div().size(px(6.0)).rounded_full().bg(accent))
            .child(div().flex_1().min_w_0().child(self.message))
            .children(self.end)
    }
}
