//! Cross-row "drag to paint" state for checkbox rows.
//!
//! When the user clicks a [`BoolRow`](super::rows::bool::BoolRow) the row
//! seeds this global with the post-flip value. Sibling rows watch the global
//! from their own `on_mouse_move` and snap to the painted value as the cursor
//! drags across them. The global self-clears the next time a mouse-move
//! arrives without the left button held, so release-anywhere needs no extra
//! window-level wiring.

use gpui::{App, Global};

pub struct DragPaint {
    target: bool,
}

impl Global for DragPaint {}

pub fn start(target: bool, cx: &mut App) {
    cx.set_global(DragPaint { target });
}

pub fn current(cx: &App) -> Option<bool> {
    cx.try_global::<DragPaint>().map(|g| g.target)
}

pub fn clear(cx: &mut App) {
    if cx.has_global::<DragPaint>() {
        cx.remove_global::<DragPaint>();
    }
}
