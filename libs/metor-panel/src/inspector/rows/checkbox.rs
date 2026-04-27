use gpui::{IntoElement, SharedString, Styled, div, prelude::*, px};

use crate::theme::Theme;

/// Toggle track-and-knob element used by [`BoolRow`].
///
/// Purely presentational; the caller owns click routing and state.
pub fn checkbox(checked: bool, theme: &Theme) -> impl IntoElement {
    let track_color = if checked {
        theme.line_colors[2].opacity(0.7)
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

/// Cell-sized tick-box used inline with value strips.
///
/// Matches the surrounding value-cell chrome so a boolean reads as another
/// cell rather than a differently-shaped toggle widget.
pub fn check_square(checked: bool, theme: &Theme) -> impl IntoElement {
    let mut square = div()
        .w(px(12.0))
        .h(px(12.0))
        .rounded(px(2.0))
        .border_1()
        .flex()
        .items_center()
        .justify_center();
    square = if checked {
        square
            .bg(theme.line_colors[2])
            .border_color(theme.line_colors[2])
            .text_size(px(10.0))
            .text_color(theme.bg_primary)
            .child(SharedString::new_static("✓"))
    } else {
        square.border_color(theme.text_tertiary)
    };
    square
}
