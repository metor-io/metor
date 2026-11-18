//! The chip that follows the pointer while a pane or tab is dragged, drawn
//! in a window of its own so the window the drag began in never clips it —
//! the drag can leave that window to tear its content out.
//!
//! gpui draws a drag ghost inside its source window only and can't place a
//! window once it is open, so the chip's window is placed through AppKit.
//! The source window keeps receiving the dragged pointer (AppKit sends a
//! drag's events to the window the press began in), and each move pins the
//! chip under it. Other platforms keep gpui's in-window ghost.

use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, DispatchPhase, Global, IntoElement,
    MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, prelude::*,
    px, size,
};

use crate::theme::CONTROL_HEIGHT;

/// Room around the chip inside its window: for its shadow, and clear of the
/// corners AppKit rounds off every window.
const MARGIN: f32 = 16.0;

struct Riding {
    /// The window the drag began in, which is fed the pointer until release.
    source: AnyWindowHandle,
    window: Option<AnyWindowHandle>,
    /// Where the pointer holds the chip's window, from its top-left.
    grab: Point<Pixels>,
}

impl Global for Riding {}

/// Whether this platform can carry the chip in a window of its own.
pub(crate) fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// Whether a drag from `source` has its chip in a window of its own, which
/// stands in for gpui's ghost.
pub(crate) fn riding(source: AnyWindowHandle, cx: &App) -> bool {
    cx.try_global::<Riding>()
        .is_some_and(|riding| riding.source == source)
}

/// A pane or tab drag just started in `window`: open its chip — `title`,
/// `width` wide when the tab's width is known — under the pointer. Reads
/// the drag from the [`ActiveDrag`] mirror, so call it once that is set.
///
/// [`ActiveDrag`]: crate::tiles::drag::ActiveDrag
pub(crate) fn begin(title: SharedString, width: Option<Pixels>, window: &Window, cx: &mut App) {
    if !supported() {
        return;
    }
    let Some(drag) = crate::tiles::drag::active_drag(cx) else {
        return;
    };
    let closable = drag.closable;
    let source = window.window_handle();
    let margin = px(MARGIN);
    let grab = drag.grab + point(margin, margin);
    let extent = size(
        width.unwrap_or(px(crate::tiles::tab_bar::TAB_MAX_WIDTH)) + margin * 2.0,
        px(CONTROL_HEIGHT) + margin * 2.0,
    );
    let bounds = Bounds {
        origin: window.bounds().origin + window.mouse_position() - grab,
        size: extent,
    };
    cx.set_global(Riding {
        source,
        window: None,
        grab,
    });
    // Deferred: opening a window mid-dispatch re-enters the platform layer.
    cx.defer(move |cx| {
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                focus: false,
                show: true,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            |window, cx| {
                native::overlay(window);
                native::pin(window, grab);
                cx.new(|_| DragChip {
                    title,
                    width,
                    closable,
                })
            },
        );
        let Ok(opened) = opened else {
            return;
        };
        let current = cx
            .try_global::<Riding>()
            .is_some_and(|riding| riding.source == source && riding.window.is_none());
        if current {
            cx.global_mut::<Riding>().window = Some(opened.into());
        } else {
            close(opened.into(), cx);
        }
    });
}

/// The drag ended: the chip goes.
pub(crate) fn finish(cx: &mut App) {
    if !cx.has_global::<Riding>() {
        return;
    }
    if let Some(window) = cx.remove_global::<Riding>().window {
        close(window, cx);
    }
}

fn close(window: AnyWindowHandle, cx: &mut App) {
    cx.defer(move |cx| {
        let _ = cx.update_window(window, |_, window, _| window.remove_window());
    });
}

/// Carry the chip with the pointer, and drop it on release, for a drag that
/// began in this window. Registered for every frame of every panel window,
/// during paint.
pub(crate) fn listen(window: &mut Window) {
    window.on_mouse_event(|_: &MouseMoveEvent, phase, window, cx| {
        if phase == DispatchPhase::Bubble {
            follow(window, cx);
        }
    });
    // In the capture phase: a drop target ends the release's propagation
    // before the bubble phase would reach this.
    window.on_mouse_event(|event: &MouseUpEvent, phase, window, cx| {
        if phase == DispatchPhase::Capture
            && event.button == MouseButton::Left
            && riding(window.window_handle(), cx)
        {
            finish(cx);
        }
    });
}

fn follow(window: &mut Window, cx: &mut App) {
    let this = window.window_handle();
    if !riding(this, cx) {
        return;
    }
    // A drag that ended without a release reaching us leaves no chip behind.
    if !cx.has_active_drag() {
        finish(cx);
        return;
    }
    let riding = cx.global::<Riding>();
    let (Some(chip), grab) = (riding.window, riding.grab) else {
        return;
    };
    let _ = cx.update_window(chip, |_, chip, _| native::pin(chip, grab));
}

/// The dragged tab, drawn as gpui's ghost would draw it.
struct DragChip {
    title: SharedString,
    /// The tab's width, when the bar measured it; otherwise it fits its
    /// title.
    width: Option<Pixels>,
    closable: bool,
}

impl Render for DragChip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let margin = px(MARGIN);
        div().size_full().relative().child(
            crate::tiles::drag::ghost_chip(self.title.clone(), self.closable, cx)
                .absolute()
                .left(margin)
                .top(margin)
                .when_some(self.width, |chip, width| chip.w(width)),
        )
    }
}

#[cfg(target_os = "macos")]
mod native {
    use gpui::{Pixels, Point, Window};
    use objc2_app_kit::{NSEvent, NSView, NSWindow, NSWindowStyleMask};
    use wgpu::rwh::{HasWindowHandle, RawWindowHandle};

    fn ns_window(window: &Window) -> Option<objc2::rc::Retained<NSWindow>> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        // gpui hands out a live NSView, borrowed only while its Window is.
        let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
        view.window()
    }

    /// Put `window`'s top-left `grab` up and left of the pointer. Placed
    /// from where the pointer is, never by how far it moved, so a move
    /// AppKit clamps (say, against the menu bar) can't leave it adrift.
    pub fn pin(window: &Window, grab: Point<Pixels>) {
        let Some(ns_window) = ns_window(window) else {
            return;
        };
        // AppKit's global space runs up from the bottom of the main
        // display, across every display.
        let pointer = NSEvent::mouseLocation();
        let mut top_left = ns_window.frame().origin;
        top_left.x = pointer.x - f64::from(f32::from(grab.x));
        top_left.y = pointer.y + f64::from(f32::from(grab.y));
        ns_window.setFrameTopLeftPoint(top_left);
    }

    /// Make `window` a pure overlay: clicks and the pointer pass through
    /// it, and it has no outline of its own (the chip draws its shadow).
    /// Borderless, too: gpui opens every window titled, and AppKit keeps a
    /// titled window below the menu bar, which would stop the chip short of
    /// the top of the screen by its margin and more.
    pub fn overlay(window: &Window) {
        if let Some(ns_window) = ns_window(window) {
            ns_window.setStyleMask(
                NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            );
            ns_window.setIgnoresMouseEvents(true);
            ns_window.setHasShadow(false);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use gpui::{Pixels, Point, Window};

    pub fn pin(_: &Window, _: Point<Pixels>) {}

    pub fn overlay(_: &Window) {}
}
