//! Shared geometry for a tab header and its floating drag preview.
use gpui::{Div, div, prelude::*, px};

use super::TabOrientation;
use crate::{icons::Icon, theme::Theme};

pub(super) const HEIGHT: f32 = 28.0;
pub(super) const RAIL_WIDTH: f32 = 160.0;

pub(super) fn header(theme: &Theme, orientation: TabOrientation) -> Div {
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(8.0))
        .h(px(HEIGHT))
        .flex_shrink_0()
        .text_size(px(12.0))
        .border_color(theme.border_primary);
    match orientation {
        TabOrientation::Horizontal => header.border_r_1(),
        TabOrientation::Vertical => header.w_full().justify_between().border_b_1(),
    }
}

pub(super) fn close_icon(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(16.0))
        .h(px(16.0))
        .flex_shrink_0()
        .rounded(px(3.0))
        .text_color(theme.text_tertiary)
        .child(Icon::Close.svg(10.0))
}
