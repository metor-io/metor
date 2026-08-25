//! Free functions over the set of open panel windows.
//!
//! There is no window manager: the window list is derived from gpui's own
//! registry on demand ([`gpui::App::windows`]), so opening and closing
//! windows needs no bookkeeping that could go stale. [`open_panel_window`]
//! is the one constructor — boot, dock reopen, and tab tear-out all go
//! through it.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Bounds, Entity, Pixels, TitlebarOptions, Window, WindowBounds,
    WindowHandle, WindowOptions, point, px, size,
};
use metor_db::DB;

use crate::app::AppRoot;
use crate::tiles::TileGroup;

/// Every open panel window, ordered by window id so iteration (layout
/// serialization, "first window" fallbacks) is deterministic in a session.
pub(crate) fn panel_windows(cx: &App) -> Vec<WindowHandle<AppRoot>> {
    let mut windows: Vec<_> = cx
        .windows()
        .into_iter()
        .filter_map(|w| w.downcast::<AppRoot>())
        .collect();
    windows.sort_by_key(|w| w.window_id());
    windows
}

/// The tile tree of the active window, falling back to the first panel
/// window. For a callback holding a `Window`, prefer [`tiles_for`] — it
/// names the window precisely instead of trusting focus.
pub(crate) fn active_tiles(cx: &App) -> Option<Entity<TileGroup>> {
    let handle = cx
        .active_window()
        .and_then(|w| w.downcast::<AppRoot>())
        .or_else(|| panel_windows(cx).into_iter().next())?;
    Some(handle.read(cx).ok()?.tiles().clone())
}

/// The tile tree owned by `window`'s root, when it is a panel window.
pub(crate) fn tiles_for(window: &Window, cx: &App) -> Option<Entity<TileGroup>> {
    let root = window.root::<AppRoot>().flatten()?;
    Some(root.read(cx).tiles().clone())
}

/// Whether any panel window holds an item: the multi-window blank-slate
/// test consent checks use before replacing what's on screen.
pub(crate) fn any_window_has_items(cx: &App) -> bool {
    panel_windows(cx)
        .iter()
        .filter_map(|w| w.read(cx).ok())
        .any(|root| root.tiles().read(cx).has_items(cx))
}

/// Open one panel window. `bounds` places it in screen coordinates
/// (sanitized against the live displays); `tiles` seeds it with an existing
/// tree — a torn-out tab, a restored layout — instead of a fresh empty
/// pane. `show_picker_if_disconnected` greets a fresh session with the
/// connection picker, wanted at boot and dock reopen but not for a window
/// that exists only to hold content.
pub(crate) fn open_panel_window(
    db: Arc<DB>,
    bounds: Option<Bounds<Pixels>>,
    tiles: Option<Entity<TileGroup>>,
    show_picker_if_disconnected: bool,
    cx: &mut App,
) -> Option<WindowHandle<AppRoot>> {
    let bounds = bounds
        .filter(|b| bounds_visible(b, &display_bounds(cx)))
        .unwrap_or_else(|| Bounds::centered(None, size(px(1024.), px(600.)), cx));
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // `appears_transparent` hides the native titlebar on macOS
            // (leaving the traffic lights) and on Windows (leaving nothing —
            // the app draws its own controls).
            titlebar: Some(TitlebarOptions {
                appears_transparent: true,
                traffic_light_position: Some(point(px(12.0), px(8.0))),
                ..Default::default()
            }),
            // Linux-only: request client-side decorations; the window root
            // wraps itself in resize borders and a shadow via
            // `window_controls::client_side_decorations`.
            window_decorations: Some(gpui::WindowDecorations::Client),
            app_id: Some("metor-panel".into()),
            ..Default::default()
        },
        move |_window, cx| cx.new(|cx| AppRoot::new(db, tiles, show_picker_if_disconnected, cx)),
    )
    .inspect_err(|err| tracing::error!(%err, "open panel window failed"))
    .ok()
}

fn display_bounds(cx: &App) -> Vec<Bounds<Pixels>> {
    cx.displays().iter().map(|d| d.bounds()).collect()
}

/// Whether any display shows at least part of `bounds` — the test that
/// keeps a window saved on a since-unplugged monitor from restoring
/// off-screen.
fn bounds_visible(bounds: &Bounds<Pixels>, displays: &[Bounds<Pixels>]) -> bool {
    displays.iter().any(|d| d.intersects(bounds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        }
    }

    #[test]
    fn bounds_on_a_live_display_are_visible() {
        let displays = [bounds(0.0, 0.0, 1920.0, 1080.0)];
        assert!(bounds_visible(&bounds(100.0, 100.0, 800.0, 600.0), &displays));
        // Partially off-screen still counts — the user can drag it back.
        assert!(bounds_visible(&bounds(1800.0, 900.0, 800.0, 600.0), &displays));
    }

    #[test]
    fn bounds_on_a_vanished_display_are_not() {
        let displays = [bounds(0.0, 0.0, 1920.0, 1080.0)];
        // A window saved on a second monitor that's gone.
        assert!(!bounds_visible(
            &bounds(2000.0, 0.0, 800.0, 600.0),
            &displays
        ));
        assert!(!bounds_visible(&bounds(0.0, 0.0, 800.0, 600.0), &[]));
    }
}
