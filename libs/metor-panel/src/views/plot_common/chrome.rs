//! Axis chrome every plot paints around its GPU frame: the opaque gutters
//! behind the tick labels, the L-shaped axis rule, and full-width or
//! full-height rules (gridlines, zero lines, limits, cursors) clipped to the
//! plot area.

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Window, point};

/// Fill the Y-label column left of `pa` and the X-label strip below it.
///
/// The fills also mask sub-pixel bleed from the GPU frame into the chrome.
pub(crate) fn paint_axis_backdrop(
    outer: Bounds<Pixels>,
    pa: Bounds<Pixels>,
    window: &mut Window,
    cx: &gpui::App,
) {
    let axis_bg = crate::theme::theme(cx).plot_chrome_bg();
    window.paint_quad(gpui::fill(
        Bounds {
            origin: outer.origin,
            size: gpui::Size {
                width: pa.origin.x - outer.origin.x,
                height: outer.size.height,
            },
        },
        axis_bg,
    ));
    window.paint_quad(gpui::fill(
        Bounds {
            origin: point(pa.origin.x, pa.origin.y + pa.size.height),
            size: gpui::Size {
                width: pa.size.width,
                height: outer.origin.y + outer.size.height - pa.origin.y - pa.size.height,
            },
        },
        axis_bg,
    ));
}

/// The L-shaped axis rule down the left edge of `pa` and along its bottom.
pub(crate) fn paint_axes(pa: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let mut axes = PathBuilder::stroke(gpui::px(1.0));
    axes.move_to(point(pa.origin.x, pa.origin.y));
    axes.line_to(point(pa.origin.x, pa.origin.y + pa.size.height));
    axes.line_to(point(
        pa.origin.x + pa.size.width,
        pa.origin.y + pa.size.height,
    ));
    if let Ok(path) = axes.build() {
        window.paint_path(path, color);
    }
}

/// A horizontal rule at `y` spanning the width of `pa`.
pub(crate) fn paint_hline(
    pa: Bounds<Pixels>,
    y: Pixels,
    width: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let mut line = PathBuilder::stroke(width);
    line.move_to(point(pa.origin.x, y));
    line.line_to(point(pa.origin.x + pa.size.width, y));
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
}

/// A vertical rule at `x` spanning the height of `pa`.
pub(crate) fn paint_vline(
    pa: Bounds<Pixels>,
    x: Pixels,
    width: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let mut line = PathBuilder::stroke(width);
    line.move_to(point(x, pa.origin.y));
    line.line_to(point(x, pa.origin.y + pa.size.height));
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
}
