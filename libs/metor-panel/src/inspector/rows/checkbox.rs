use gpui::{IntoElement, SharedString, Styled, div, prelude::*, px};

use crate::theme::Theme;

/// Compact toggle track+knob used by [`BoolRow`]. Stateless — the caller
/// decides what to do on click.
pub fn checkbox(checked: bool, theme: &Theme) -> impl IntoElement {
    let track_color = if checked {
        theme.line_color
    } else {
        theme.text_tertiary
    };
    let knob = div()
        .w(px(10.0))
        .h(px(10.0))
        .rounded(px(2.0))
        .bg(theme.text_primary);
    div()
        .w(px(28.0))
        .h(px(14.0))
        .rounded(px(3.0))
        .bg(track_color)
        .flex()
        .items_center()
        .px(px(2.0))
        .when(checked, |el| el.flex_row_reverse())
        .child(knob)
}

/// Compact tick-box sized to match the cell's text chrome — a 12×12 square
/// with 2px rounding, filled with the theme accent when checked. Used inline
/// in [`ComponentValueStrip`] so a bool cell reads as the same kind of box
/// as its surrounding value cell, not a separate toggle widget.
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
            .bg(theme.line_color)
            .border_color(theme.line_color)
            .text_size(px(10.0))
            .text_color(theme.bg_primary)
            .child(SharedString::new_static("✓"))
    } else {
        square.border_color(theme.text_tertiary)
    };
    square
}
