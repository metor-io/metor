//! Shared geometry for a tab header and its floating drag preview.
use gpui::{Div, SharedString, div, prelude::*, px};

use super::TabOrientation;
use crate::{icons::Icon, theme::Theme};

pub(super) const HEIGHT: f32 = 28.0;
/// A horizontal tab sizes to its title up to this width, then ellipsizes so
/// one long title can't crowd every other tab off the strip.
pub(super) const MAX_WIDTH: f32 = 240.0;
pub(super) const RAIL_WIDTH: f32 = 160.0;
pub(super) const MIN_RAIL_WIDTH: f32 = 80.0;
pub(super) const MAX_RAIL_WIDTH: f32 = 480.0;
/// Grab area of the vertical rail's resize edge. Wider than the 1px border it
/// sits on, and kept inside the rail so the pane content can't occlude it.
pub(super) const RAIL_HANDLE_WIDTH: f32 = 5.0;

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
        TabOrientation::Horizontal => header.max_w(px(MAX_WIDTH)).border_r_1(),
        TabOrientation::Vertical => header.w_full().justify_between().border_b_1(),
    }
}

/// The title shrinks before the close button does, and ellipsizes rather
/// than clipping mid-glyph.
pub(super) fn title(title: SharedString) -> Div {
    div().flex_shrink(1.0).min_w_0().truncate().child(title)
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
        .child(Icon::Close.svg(10.0, theme.text_secondary))
}
