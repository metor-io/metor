use gpui::{IntoElement, Styled, div, prelude::*, px};

use crate::theme::Theme;

/// Toggle track-and-knob element used by [`BoolRow`].
///
/// Purely presentational; the caller owns click routing and state.
pub fn checkbox(checked: bool, theme: &Theme) -> impl IntoElement {
    let track_color = if checked {
        theme.control_active_track()
    } else {
        theme.text_tertiary
    };
    let knob = div()
        .w(px(10.0))
        .h(px(10.0))
        .rounded(px(2.0))
        .bg(theme.text_primary);
    div()
        .w(px(22.0))
        .h(px(14.0))
        .rounded(px(3.0))
        .bg(track_color)
        .flex()
        .items_center()
        .px(px(2.0))
        .when(checked, |el| el.flex_row_reverse())
        .child(knob)
}
