//! Shared appearance for timeline and time-series readouts.

use gpui::{App, Div, div, prelude::*, px};

use crate::theme::{font_family, theme};
use crate::views::time_series::LABEL_FONT_SIZE;

/// Inner padding also used when estimating readout placement before layout.
pub(crate) const READOUT_PAD_X: f32 = 6.0;
pub(crate) const READOUT_PAD_Y: f32 = 4.0;

/// Readouts can render in the window's tooltip layer, outside the app's
/// inherited text style, so their font must be set explicitly.
pub(crate) fn readout_card(cx: &App) -> Div {
    let theme = theme(cx);
    div()
        .flex()
        .flex_col()
        .gap_y_0()
        .px(px(READOUT_PAD_X))
        .py(px(READOUT_PAD_Y))
        .bg(theme.bg_elevated)
        .border_1()
        .border_color(theme.border_primary)
        .rounded(px(3.0))
        .font_family(font_family(cx))
        .text_size(px(LABEL_FONT_SIZE))
        .text_color(theme.text_secondary)
}
