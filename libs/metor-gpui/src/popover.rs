//! The card readouts, such as a plot cursor's value, are drawn on.

use gpui::{App, Div, div, prelude::*, px};

use crate::{font_family, theme, ui_px};

/// Inner padding also used when estimating readout placement before layout.
pub const READOUT_PAD_X: f32 = 6.0;
pub const READOUT_PAD_Y: f32 = 4.0;

/// Readouts can render in the window's tooltip layer, outside the app's
/// inherited text style, so their font must be set explicitly.
pub fn readout_card(font_size: f32, cx: &App) -> Div {
    let theme = theme(cx);
    div()
        .flex()
        .flex_col()
        .gap_y_0()
        .px(ui_px(READOUT_PAD_X))
        .py(ui_px(READOUT_PAD_Y))
        .bg(theme.surface_bg())
        .backdrop_blur(crate::SURFACE_BLUR)
        .border_1()
        .border_color(theme.border_primary)
        .rounded(px(3.0))
        .font_family(font_family(cx))
        .text_size(ui_px(font_size))
        .text_color(theme.text_secondary)
}
