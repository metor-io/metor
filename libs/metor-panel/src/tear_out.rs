//! While a pane or tab is dragged outside its window, a miniature of the
//! window it would tear out into rides the pointer. Nothing has moved yet:
//! bring the pointer back and the drag carries on as before; release
//! outside and the window root tears the content out for real.
//!
//! gpui draws a drag preview inside its source window only and can't place
//! a window once it is open, so the miniature is a window of its own moved
//! through AppKit. The source window keeps receiving the dragged pointer
//! (AppKit sends a drag's events to the window the press began in), and
//! each move shifts the miniature by the pointer's travel. Other platforms
//! go without the miniature.

use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, DispatchPhase, Global, IntoElement,
    MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Render, SharedString, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, prelude::*, px,
    relative,
};

use crate::theme::theme;
use crate::tiles::Sketch;

/// Width of the miniature; its height follows the torn window's shape.
pub(crate) const PREVIEW_WIDTH: f32 = 240.0;
const TITLEBAR: f32 = 18.0;
/// Where the pointer holds the miniature: on its titlebar, near the left.
pub(crate) const GRAB: (f32, f32) = (28.0, 9.0);

struct Riding {
    /// The window the drag began in, which is fed the pointer until release.
    source: AnyWindowHandle,
    /// Tells a miniature that finishes opening after its drag left the
    /// desktop (or left and came back) that it is stale.
    generation: u64,
    window: Option<AnyWindowHandle>,
    /// The pointer, in global screen coordinates, when the miniature was
    /// last placed.
    cursor: Option<(f64, f64)>,
}

impl Global for Riding {}

/// Whether this platform can carry a miniature under the pointer.
pub(crate) fn supported() -> bool {
    cfg!(target_os = "macos")
}

pub(crate) fn showing(source: AnyWindowHandle, cx: &App) -> bool {
    cx.try_global::<Riding>()
        .is_some_and(|riding| riding.source == source)
}

/// Open the miniature at `bounds` (screen coordinates), drawing `title`'s
/// layout, for a drag from `source`.
pub(crate) fn show(
    source: AnyWindowHandle,
    bounds: Bounds<Pixels>,
    title: SharedString,
    sketch: Sketch,
    cx: &mut App,
) {
    let generation = cx
        .try_global::<Riding>()
        .map_or(0, |riding| riding.generation + 1);
    cx.set_global(Riding {
        source,
        generation,
        window: None,
        cursor: native::mouse_location(),
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
                native::ignore_mouse(window);
                cx.new(|_| DragOutPreview { title, sketch })
            },
        );
        let Ok(opened) = opened else {
            return;
        };
        let current = cx
            .try_global::<Riding>()
            .is_some_and(|riding| riding.generation == generation && riding.window.is_none());
        if current {
            cx.global_mut::<Riding>().window = Some(opened.into());
        } else {
            close(opened.into(), cx);
        }
    });
}

/// Put the miniature away: the pointer came back, or the drag ended.
pub(crate) fn hide(cx: &mut App) {
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

/// Carry the miniature with the pointer, and drop it on release, for a
/// drag that began in this window. Registered for every frame of every
/// panel window, during paint.
pub(crate) fn listen(window: &mut Window) {
    window.on_mouse_event(|_: &MouseMoveEvent, phase, window, cx| {
        if phase == DispatchPhase::Bubble {
            follow(window, cx);
        }
    });
    window.on_mouse_event(|event: &MouseUpEvent, phase, window, cx| {
        if phase == DispatchPhase::Bubble
            && event.button == MouseButton::Left
            && showing(window.window_handle(), cx)
        {
            hide(cx);
        }
    });
}

fn follow(window: &mut Window, cx: &mut App) {
    let this = window.window_handle();
    let Some(riding) = cx
        .try_global::<Riding>()
        .filter(|riding| riding.source == this)
    else {
        return;
    };
    let (Some(preview), Some(last), Some(now)) =
        (riding.window, riding.cursor, native::mouse_location())
    else {
        return;
    };
    let travel = (now.0 - last.0, now.1 - last.1);
    let _ = cx.update_window(preview, |_, preview, _| native::move_by(preview, travel));
    cx.global_mut::<Riding>().cursor = Some(now);
}

/// The window a drag would tear out, in miniature: its titlebar with the
/// tab's name, then its panes in proportion, each with its header.
struct DragOutPreview {
    title: SharedString,
    sketch: Sketch,
}

impl Render for DragOutPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .when_some(
                cx.try_global::<crate::theme::FontSettings>()
                    .map(|font| font.family.clone()),
                |preview, family| preview.font_family(family),
            )
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border_primary)
            .bg(theme.bg_secondary)
            .opacity(0.94)
            .child(
                div()
                    .h(px(TITLEBAR))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px(px(6.0))
                    .child(
                        div()
                            .px(px(6.0))
                            .rounded(px(3.0))
                            .bg(theme.bg_primary)
                            .border_1()
                            .border_color(theme.border_primary)
                            .text_size(px(9.0))
                            .text_color(theme.text_primary)
                            .max_w(relative(0.6))
                            .truncate()
                            .child(self.title.clone()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .p(px(3.0))
                    .pt_0()
                    .child(render_sketch(&self.sketch, cx)),
            )
    }
}

fn render_sketch(sketch: &Sketch, cx: &App) -> gpui::AnyElement {
    let theme = theme(cx);
    match sketch {
        Sketch::Pane(title) => div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(3.0))
            .bg(theme.bg_primary)
            .child(
                div()
                    .h(px(12.0))
                    .flex_shrink_0()
                    .px(px(4.0))
                    .text_size(px(8.0))
                    .text_color(theme.text_secondary)
                    .truncate()
                    .child(title.clone()),
            )
            .into_any_element(),
        Sketch::Split { axis, parts } => {
            let mut split = div().size_full().flex().gap(px(2.0));
            if *axis == gpui::Axis::Vertical {
                split = split.flex_col();
            }
            split
                .children(parts.iter().map(|(flex, part)| {
                    let mut slot = div()
                        .flex_basis(relative(0.))
                        .min_w_0()
                        .min_h_0()
                        .flex()
                        .child(render_sketch(part, cx));
                    let style = slot.style();
                    style.flex_grow = Some(*flex);
                    style.flex_shrink = Some(*flex);
                    slot
                }))
                .into_any_element()
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use gpui::Window;
    use objc2_app_kit::{NSEvent, NSView, NSWindow};
    use wgpu::rwh::{HasWindowHandle, RawWindowHandle};

    /// The pointer in AppKit's global screen space (y up, spanning every
    /// display), so a miniature carried across displays never jumps.
    pub fn mouse_location() -> Option<(f64, f64)> {
        let point = NSEvent::mouseLocation();
        Some((point.x, point.y))
    }

    fn ns_window(window: &Window) -> Option<objc2::rc::Retained<NSWindow>> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        // gpui hands out a live NSView, borrowed only while its Window is.
        let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
        view.window()
    }

    /// Shift `window` by `travel`, measured in the same space as
    /// [`mouse_location`].
    pub fn move_by(window: &Window, (dx, dy): (f64, f64)) {
        if let Some(ns_window) = ns_window(window) {
            let mut origin = ns_window.frame().origin;
            origin.x += dx;
            origin.y += dy;
            ns_window.setFrameOrigin(origin);
        }
    }

    /// Let clicks and the pointer pass through to whatever lies beneath.
    pub fn ignore_mouse(window: &Window) {
        if let Some(ns_window) = ns_window(window) {
            ns_window.setIgnoresMouseEvents(true);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    pub fn mouse_location() -> Option<(f64, f64)> {
        None
    }

    pub fn move_by(_: &gpui::Window, _: (f64, f64)) {}

    pub fn ignore_mouse(_: &gpui::Window) {}
}
