//! Buttons: square icon buttons and the text buttons of chip headers.

use gpui::{Div, ElementId, Hsla, SharedString, Stateful, div, prelude::*, px};

use crate::Icon;
use crate::TooltipText;
use crate::table::HEADER_CONTROL_HEIGHT;
use crate::{Theme, ui_px};

/// Icon buttons taller than this draw the larger icon. It is the height of the panel's value
/// boxes, which icon buttons in a row of them share.
const LARGE_ICON_HEIGHT: f32 = 25.5;

/// A square button `height` tall showing `icon`, named by `tooltip` on
/// hover. Inert until [`pressable`] makes it act on click.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: Icon,
    color: Hsla,
    bg: Hsla,
    tooltip: impl Into<SharedString>,
    height: f32,
) -> Stateful<Div> {
    let id = id.into();
    let selector = id.to_string();
    let tooltip = tooltip.into();
    div()
        .id(id)
        .debug_selector(move || selector)
        .flex_none()
        .size(ui_px(height))
        .flex()
        .items_center()
        .justify_center()
        .rounded(ui_px(4.0))
        .bg(bg)
        .tooltip(move |_window, cx| TooltipText::build(tooltip.clone(), cx))
        .child(icon.svg(
            if height > LARGE_ICON_HEIGHT {
                15.0
            } else {
                13.0
            },
            color,
        ))
}

/// The pointer and hover tint of an [`icon_button`] that acts on click.
/// Inert ones (a command the channel won't take now) keep neither, so they
/// don't read as pressable.
pub fn pressable(button: Stateful<Div>, theme: &Theme) -> Stateful<Div> {
    let hover = theme.selection_bg;
    button.cursor_pointer().hover(move |s| s.bg(hover))
}

/// A text button in a chip header, the caller adding its id and click.
pub fn header_button(label: impl Into<SharedString>, theme: &Theme) -> Div {
    div()
        .h(px(HEADER_CONTROL_HEIGHT))
        .flex()
        .flex_none()
        .items_center()
        .px_2()
        .rounded(px(3.0))
        .bg(theme.bg_elevated)
        .text_color(theme.text_secondary)
        .text_size(px(11.0))
        .cursor_pointer()
        .hover(|style| style.text_color(theme.text_primary))
        .child(label.into())
}
