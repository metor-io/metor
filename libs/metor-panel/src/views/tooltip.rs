//! Minimal text tooltip widget.
//!
//! gpui's `tooltip` builder must return an `AnyView`, but the panel doesn't
//! depend on zed's `ui::Tooltip`. This fills that gap so any view can attach
//! a one-line label without pulling in heavier UI dependencies.

use gpui::{AnyView, App, Context, IntoElement, Render, SharedString, Window, div, prelude::*, px};

use crate::theme::theme;

pub struct TooltipText {
    text: SharedString,
}

impl TooltipText {
    pub fn build(text: SharedString, cx: &mut App) -> AnyView {
        cx.new(|_cx| Self { text }).into()
    }
}

impl Render for TooltipText {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .text_size(px(11.0))
            .text_color(theme.text_primary)
            .child(self.text.clone())
    }
}
