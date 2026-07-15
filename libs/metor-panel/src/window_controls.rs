//! Window chrome the OS doesn't draw for us: caption buttons and Linux
//! client-side decorations.
//!
//! macOS keeps its native traffic lights (`appears_transparent` only hides the
//! bar behind them). Windows hides the whole native titlebar, so the panel
//! draws minimize/maximize/close itself; tagging each with a
//! [`WindowControlArea`] routes them through the OS non-client hit-test, which
//! also performs the click natively. On Linux the panel requests client-side
//! decorations, so beyond the buttons it owns the resize border, corner
//! rounding, and drop shadow — [`client_side_decorations`] wraps the window
//! root with all three, and collapses to a plain container wherever the
//! compositor decorates for us.
//!
//! Everything here branches on `cfg!()` rather than `#[cfg]` so the whole
//! module type-checks on every platform.

use gpui::prelude::FluentBuilder;
use gpui::{
    App, Bounds, BoxShadow, CursorStyle, Decorations, Div, Global, HitboxBehavior,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Point, ResizeEdge, Size,
    Stateful, Styled, Tiling, Window, WindowControlArea, canvas, div, point, px, size,
    transparent_black,
};

use crate::icons::Icon;
use crate::theme::Theme;

const CAPTION_BUTTON_WIDTH: Pixels = px(44.);
const CSD_ROUNDING: Pixels = px(10.);
const CSD_SHADOW: Pixels = px(10.);
const CSD_BORDER: Pixels = px(1.);

/// True when this window must draw its own minimize/maximize/close buttons.
pub(crate) fn needs_window_controls(window: &Window) -> bool {
    if cfg!(target_os = "macos") || window.is_fullscreen() {
        return false;
    }
    if cfg!(target_os = "windows") {
        // `appears_transparent` hides the native titlebar; ours is the only one.
        return true;
    }
    // Compositors that refuse server-side decorations (GNOME Wayland) report
    // client decorations regardless of what the window requested.
    matches!(window.window_decorations(), Decorations::Client { .. })
}

/// The minimize / maximize-or-restore / close button row for platforms
/// without native controls. Occluded so the buttons win the non-client
/// hit-test over the surrounding titlebar drag area.
pub(crate) fn window_controls(theme: &Theme, window: &Window) -> impl IntoElement {
    let maximize_icon = if window.is_maximized() {
        Icon::Restore
    } else {
        Icon::Maximize
    };
    // The mouse-up handlers are skipped on Windows: the non-client hit-test
    // already performs min/max/close natively, and gpui replays NC mouse
    // events into the client pipeline, so acting here too would double-fire
    // (maximize would toggle twice and appear dead).
    div()
        .occlude()
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .child(
            caption_button("window-minimize", Icon::Subtract, WindowControlArea::Min, theme)
                .hover(|s| s.bg(theme.bg_primary))
                .on_mouse_up(MouseButton::Left, |_, window, _| {
                    if !cfg!(target_os = "windows") {
                        window.minimize_window();
                    }
                }),
        )
        .child(
            caption_button("window-maximize", maximize_icon, WindowControlArea::Max, theme)
                .hover(|s| s.bg(theme.bg_primary))
                .on_mouse_up(MouseButton::Left, |_, window, _| {
                    if !cfg!(target_os = "windows") {
                        window.zoom_window();
                    }
                }),
        )
        .child(
            caption_button("window-close", Icon::Close, WindowControlArea::Close, theme)
                .hover(|s| s.bg(theme.error_accent))
                .on_mouse_up(MouseButton::Left, |_, window, _| {
                    if !cfg!(target_os = "windows") {
                        window.remove_window();
                    }
                }),
        )
}

fn caption_button(
    id: &'static str,
    icon: Icon,
    area: WindowControlArea,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .window_control_area(area)
        .flex()
        .items_center()
        .justify_center()
        .w(CAPTION_BUTTON_WIDTH)
        .h_full()
        .child(icon.svg_color(14.0, theme.text_secondary))
}

/// Wrap the window root with Linux client-side decorations: an invisible
/// resize border (mouse-down starts a compositor resize), a drop shadow and
/// rounded corners when floating, and per-edge suppression when tiled.
///
/// Ported from Zed's `client_side_decorations`. Under server decorations
/// (macOS, Windows, Linux compositors that decorate) this is a plain
/// full-size container with zero visual effect.
pub(crate) fn client_side_decorations(
    element: impl IntoElement,
    window: &mut Window,
    cx: &mut App,
) -> Stateful<Div> {
    let theme = crate::theme::theme(cx);
    let decorations = window.window_decorations();
    let tiling = match decorations {
        Decorations::Server => Tiling::default(),
        Decorations::Client { tiling } => tiling,
    };

    match decorations {
        Decorations::Client { .. } => window.set_client_inset(CSD_SHADOW),
        Decorations::Server => window.set_client_inset(px(0.0)),
    }

    struct GlobalResizeEdge(ResizeEdge);
    impl Global for GlobalResizeEdge {}

    div()
        .id("window-backdrop")
        .bg(transparent_black())
        .map(|div| match decorations {
            Decorations::Server => div,
            Decorations::Client { .. } => div
                .when(!(tiling.top || tiling.right), |div| {
                    div.rounded_tr(CSD_ROUNDING)
                })
                .when(!(tiling.top || tiling.left), |div| {
                    div.rounded_tl(CSD_ROUNDING)
                })
                .when(!(tiling.bottom || tiling.right), |div| {
                    div.rounded_br(CSD_ROUNDING)
                })
                .when(!(tiling.bottom || tiling.left), |div| {
                    div.rounded_bl(CSD_ROUNDING)
                })
                .when(!tiling.top, |div| div.pt(CSD_SHADOW))
                .when(!tiling.bottom, |div| div.pb(CSD_SHADOW))
                .when(!tiling.left, |div| div.pl(CSD_SHADOW))
                .when(!tiling.right, |div| div.pr(CSD_SHADOW))
                .on_mouse_move(move |e, window, cx| {
                    let size = window.window_bounds().get_bounds().size;
                    let new_edge = resize_edge(e.position, CSD_SHADOW, size, tiling);
                    let edge = cx.try_global::<GlobalResizeEdge>();
                    if new_edge != edge.map(|edge| edge.0) {
                        window
                            .window_handle()
                            .update(cx, |root, _, cx| cx.notify(root.entity_id()))
                            .ok();
                    }
                })
                .on_mouse_down(MouseButton::Left, move |e, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    let Some(edge) = resize_edge(e.position, CSD_SHADOW, size, tiling) else {
                        return;
                    };
                    window.start_window_resize(edge);
                }),
        })
        .size_full()
        .child(
            div()
                .cursor(CursorStyle::Arrow)
                .map(|div| match decorations {
                    Decorations::Server => div,
                    Decorations::Client { .. } => div
                        .border_color(theme.border_primary)
                        .when(!(tiling.top || tiling.right), |div| {
                            div.rounded_tr(CSD_ROUNDING)
                        })
                        .when(!(tiling.top || tiling.left), |div| {
                            div.rounded_tl(CSD_ROUNDING)
                        })
                        .when(!(tiling.bottom || tiling.right), |div| {
                            div.rounded_br(CSD_ROUNDING)
                        })
                        .when(!(tiling.bottom || tiling.left), |div| {
                            div.rounded_bl(CSD_ROUNDING)
                        })
                        .when(!tiling.top, |div| div.border_t(CSD_BORDER))
                        .when(!tiling.bottom, |div| div.border_b(CSD_BORDER))
                        .when(!tiling.left, |div| div.border_l(CSD_BORDER))
                        .when(!tiling.right, |div| div.border_r(CSD_BORDER))
                        .when(!tiling.is_tiled(), |div| {
                            div.shadow(vec![BoxShadow {
                                color: theme.window_shadow(),
                                blur_radius: CSD_SHADOW / 2.,
                                spread_radius: px(0.),
                                offset: point(px(0.0), px(0.0)),
                            }])
                        }),
                })
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .size_full()
                .child(element),
        )
        .map(|div| match decorations {
            Decorations::Server => div,
            Decorations::Client { tiling, .. } => div.child(
                canvas(
                    |_bounds, window, _| {
                        window.insert_hitbox(
                            Bounds::new(
                                point(px(0.0), px(0.0)),
                                window.window_bounds().get_bounds().size,
                            ),
                            HitboxBehavior::Normal,
                        )
                    },
                    move |_bounds, hitbox, window, cx| {
                        let mouse = window.mouse_position();
                        let size = window.window_bounds().get_bounds().size;
                        let Some(edge) = resize_edge(mouse, CSD_SHADOW, size, tiling) else {
                            return;
                        };
                        cx.set_global(GlobalResizeEdge(edge));
                        window.set_cursor_style(
                            match edge {
                                ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
                                ResizeEdge::Left | ResizeEdge::Right => {
                                    CursorStyle::ResizeLeftRight
                                }
                                ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                                    CursorStyle::ResizeUpLeftDownRight
                                }
                                ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                                    CursorStyle::ResizeUpRightDownLeft
                                }
                            },
                            &hitbox,
                        );
                    },
                )
                .size_full()
                .absolute(),
            ),
        })
}

/// Which resize edge, if any, the cursor is over inside the invisible
/// client-inset border. Corners claim a 1.5x-shadow square; tiled edges are
/// suppressed because the compositor owns them.
fn resize_edge(
    pos: Point<Pixels>,
    shadow_size: Pixels,
    window_size: Size<Pixels>,
    tiling: Tiling,
) -> Option<ResizeEdge> {
    let bounds = Bounds::new(Point::default(), window_size).inset(shadow_size * 1.5);
    if bounds.contains(&pos) {
        return None;
    }

    let corner_size = size(shadow_size * 1.5, shadow_size * 1.5);
    let top_left_bounds = Bounds::new(Point::new(px(0.), px(0.)), corner_size);
    if !tiling.top && top_left_bounds.contains(&pos) {
        return Some(ResizeEdge::TopLeft);
    }

    let top_right_bounds = Bounds::new(
        Point::new(window_size.width - corner_size.width, px(0.)),
        corner_size,
    );
    if !tiling.top && top_right_bounds.contains(&pos) {
        return Some(ResizeEdge::TopRight);
    }

    let bottom_left_bounds = Bounds::new(
        Point::new(px(0.), window_size.height - corner_size.height),
        corner_size,
    );
    if !tiling.bottom && bottom_left_bounds.contains(&pos) {
        return Some(ResizeEdge::BottomLeft);
    }

    let bottom_right_bounds = Bounds::new(
        Point::new(
            window_size.width - corner_size.width,
            window_size.height - corner_size.height,
        ),
        corner_size,
    );
    if !tiling.bottom && bottom_right_bounds.contains(&pos) {
        return Some(ResizeEdge::BottomRight);
    }

    if !tiling.top && pos.y < shadow_size {
        Some(ResizeEdge::Top)
    } else if !tiling.bottom && pos.y > window_size.height - shadow_size {
        Some(ResizeEdge::Bottom)
    } else if !tiling.left && pos.x < shadow_size {
        Some(ResizeEdge::Left)
    } else if !tiling.right && pos.x > window_size.width - shadow_size {
        Some(ResizeEdge::Right)
    } else {
        None
    }
}
