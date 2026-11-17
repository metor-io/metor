//! A pane or tab dragged out of its window becomes a window of its own the
//! moment the pointer leaves, and that window rides the pointer until the
//! button comes up — the user carries the real, live content rather than a
//! picture of it.
//!
//! gpui draws a drag preview inside its source window only and can't place
//! a window once it is open, so this is AppKit's: the source window keeps
//! receiving the dragged pointer (AppKit sends a drag's events to the
//! window the press began in), and each move shifts the torn window's
//! `NSWindow` by the pointer's travel. Elsewhere the drag still tears out
//! on release, from the window root's mouse-up.

use gpui::{
    AnyWindowHandle, App, AppContext, DispatchPhase, Global, MouseButton, MouseMoveEvent,
    MouseUpEvent, Window,
};

struct TearOut {
    /// The window the drag began in, which is fed the pointer until release.
    source: AnyWindowHandle,
    /// The window riding the pointer: the new one once it has opened, or the
    /// source itself when the drag carried everything it held.
    torn: Option<AnyWindowHandle>,
    /// The pointer, in global screen coordinates, when the torn window was
    /// last placed.
    cursor: Option<(f64, f64)>,
}

impl Global for TearOut {}

/// Whether this platform can carry a torn window under the pointer.
pub(crate) fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// Start carrying `torn` (or, while `None`, the window about to open for
/// the drag) from `source`, from wherever the pointer is now.
pub(crate) fn begin(source: AnyWindowHandle, torn: Option<AnyWindowHandle>, cx: &mut App) {
    cx.set_global(TearOut {
        source,
        torn,
        cursor: native::mouse_location(),
    });
}

/// The torn window has opened. It rides the pointer from here, or, when the
/// button already came up while it was opening, just comes forward.
pub(crate) fn opened(window: AnyWindowHandle, cx: &mut App) {
    match cx.try_global::<TearOut>() {
        Some(tear) if tear.torn.is_none() => cx.global_mut::<TearOut>().torn = Some(window),
        _ => activate(window, cx),
    }
}

/// Follow the pointer for a tear-out that began in this window. Registered
/// for every frame of every panel window, during paint.
pub(crate) fn listen(window: &mut Window) {
    window.on_mouse_event(|_: &MouseMoveEvent, phase, window, cx| {
        if phase == DispatchPhase::Bubble {
            follow(window, cx);
        }
    });
    window.on_mouse_event(|event: &MouseUpEvent, phase, window, cx| {
        if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
            finish(window, cx);
        }
    });
}

fn follow(window: &mut Window, cx: &mut App) {
    let this = window.window_handle();
    let Some(tear) = cx
        .try_global::<TearOut>()
        .filter(|tear| tear.source == this)
    else {
        return;
    };
    let (Some(torn), Some(last), Some(now)) = (tear.torn, tear.cursor, native::mouse_location())
    else {
        return;
    };
    let travel = (now.0 - last.0, now.1 - last.1);
    if torn == this {
        native::move_by(window, travel);
    } else {
        let _ = cx.update_window(torn, |_, torn, _| native::move_by(torn, travel));
    }
    cx.global_mut::<TearOut>().cursor = Some(now);
}

fn finish(window: &mut Window, cx: &mut App) {
    let this = window.window_handle();
    if !cx
        .try_global::<TearOut>()
        .is_some_and(|tear| tear.source == this)
    {
        return;
    }
    let tear = cx.remove_global::<TearOut>();
    if let Some(torn) = tear.torn
        && torn != this
    {
        activate(torn, cx);
    }
}

fn activate(window: AnyWindowHandle, cx: &mut App) {
    let _ = cx.update_window(window, |_, window, _| window.activate_window());
}

#[cfg(target_os = "macos")]
mod native {
    use gpui::Window;
    use objc2_app_kit::{NSEvent, NSView};
    use wgpu::rwh::{HasWindowHandle, RawWindowHandle};

    /// The pointer in AppKit's global screen space (y up, spanning every
    /// display), so a window carried across displays never jumps.
    pub fn mouse_location() -> Option<(f64, f64)> {
        let point = NSEvent::mouseLocation();
        Some((point.x, point.y))
    }

    /// Shift `window` by `travel`, measured in the same space as
    /// [`mouse_location`].
    pub fn move_by(window: &Window, (dx, dy): (f64, f64)) {
        let Ok(handle) = HasWindowHandle::window_handle(window) else {
            return;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return;
        };
        // gpui hands out a live NSView, borrowed only while its Window is.
        let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
        let Some(ns_window) = view.window() else {
            return;
        };
        let mut origin = ns_window.frame().origin;
        origin.x += dx;
        origin.y += dy;
        ns_window.setFrameOrigin(origin);
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    pub fn mouse_location() -> Option<(f64, f64)> {
        None
    }

    pub fn move_by(_: &gpui::Window, _: (f64, f64)) {}
}
