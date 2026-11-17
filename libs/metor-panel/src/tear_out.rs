//! While a pane or tab is dragged outside its window, the window it would
//! tear out into rides the pointer: its live panes, grown out of the drag
//! ghost. Nothing has moved yet: bring the pointer back and the preview
//! shrinks back into the ghost and the drag carries on; release outside
//! and the window root tears the content out for real, where the preview
//! stood.
//!
//! gpui draws a drag ghost inside its source window only and can't place a
//! window once it is open, so the preview is a window of its own moved
//! through AppKit. The source window keeps receiving the dragged pointer
//! (AppKit sends a drag's events to the window the press began in), and
//! each move shifts the preview by the pointer's travel. Other platforms go
//! without the preview.
//!
//! The preview draws the panes' own views a second time, so it can't be a
//! scaled picture: gpui has no transform for live elements and no readback
//! of a window's pixels outside tests. At full size the views draw at the
//! size they already have; the mini preview lays them out small.

use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, DispatchPhase, Global, IntoElement,
    MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Size, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, point,
    prelude::*, px, relative, size,
};

use crate::config::DragPreview;
use crate::motion::Fade;
use crate::theme::{CONTROL_HEIGHT, CONTROL_RADIUS, theme};
use crate::tiles::Replica;

const GROW: Duration = Duration::from_millis(180);
const SHRINK: Duration = Duration::from_millis(120);
/// Width the mini preview scales a wider window down to.
const MINI_WIDTH: f32 = 420.0;

struct Riding {
    /// The window the drag began in, which is fed the pointer until release.
    source: AnyWindowHandle,
    window: Option<WindowHandle<DragOutPreview>>,
    /// Out of the source window, so the preview shows and the ghost hides.
    expanded: bool,
    /// The pointer, in global screen coordinates, when the preview was last
    /// placed.
    cursor: Option<(f64, f64)>,
}

impl Global for Riding {}

/// Whether this platform can carry a preview under the pointer.
pub(crate) fn supported() -> bool {
    cfg!(target_os = "macos")
}

pub(crate) fn preference(cx: &App) -> DragPreview {
    cx.try_global::<crate::theme::FontSettings>()
        .map(|settings| settings.config.drag_preview)
        .unwrap_or_default()
}

pub(crate) fn set_preference(preview: DragPreview, cx: &mut App) {
    let settings = cx.global_mut::<crate::theme::FontSettings>();
    settings.config.drag_preview = preview;
    if let Err(error) = crate::config::save(&settings.config) {
        tracing::error!(%error, "save drag preview failed");
    }
}

/// The preview's size for a window of `torn` size.
pub(crate) fn preview_size(torn: Size<Pixels>, cx: &App) -> Size<Pixels> {
    match preference(cx) {
        DragPreview::Full => torn,
        DragPreview::Mini => {
            let scale = (MINI_WIDTH / f32::from(torn.width)).min(1.0);
            size(torn.width * scale, torn.height * scale)
        }
    }
}

/// Where the pointer holds a preview (or a torn window) of `extent`: where
/// it held the ghost, kept on the window's titlebar.
pub(crate) fn grab_point(grab: Point<Pixels>, extent: Size<Pixels>) -> Point<Pixels> {
    point(
        grab.x.clamp(px(12.0), (extent.width * 0.5).max(px(12.0))),
        grab.y.clamp(px(8.0), px(crate::app::TITLEBAR_HEIGHT - 8.0)),
    )
}

/// Whether a drag from `source` has its preview out, which hides the ghost.
pub(crate) fn expanded(source: AnyWindowHandle, cx: &App) -> bool {
    cx.try_global::<Riding>()
        .is_some_and(|riding| riding.source == source && riding.expanded)
}

/// Whether a drag from `source` has opened a preview, out or tucked away.
pub(crate) fn riding(source: AnyWindowHandle, cx: &App) -> bool {
    cx.try_global::<Riding>()
        .is_some_and(|riding| riding.source == source)
}

/// What a preview shows and where: `bounds` on screen, growing out of the
/// ghost at `ghost` (window coordinates).
pub(crate) struct Spec {
    pub bounds: Bounds<Pixels>,
    pub ghost: Bounds<Pixels>,
    pub title: SharedString,
    pub replica: Replica,
}

/// Open the preview for a drag from `source`, growing out of its ghost.
pub(crate) fn open(source: AnyWindowHandle, spec: Spec, cx: &mut App) {
    cx.set_global(Riding {
        source,
        window: None,
        expanded: true,
        cursor: native::mouse_location(),
    });
    // Deferred: opening a window mid-dispatch re-enters the platform layer.
    cx.defer(move |cx| {
        let Spec {
            bounds,
            ghost,
            title,
            replica,
        } = spec;
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
                cx.new(|_| DragOutPreview {
                    title,
                    replica,
                    ghost,
                    fade: Fade::entrance(GROW),
                })
            },
        );
        let Ok(opened) = opened else {
            return;
        };
        let state = cx
            .try_global::<Riding>()
            .filter(|riding| riding.source == source && riding.window.is_none())
            .map(|riding| riding.expanded);
        match state {
            Some(expanded) => {
                cx.global_mut::<Riding>().window = Some(opened);
                if !expanded {
                    set_expanded(false, cx);
                }
            }
            None => close(opened.into(), cx),
        }
    });
}

/// Grow the preview back out of the ghost, or shrink it into it.
pub(crate) fn set_expanded(expanded: bool, cx: &mut App) {
    if !cx.has_global::<Riding>() {
        return;
    }
    let riding = cx.global_mut::<Riding>();
    riding.expanded = expanded;
    if let Some(window) = riding.window {
        let _ = window.update(cx, |preview, _, cx| {
            if expanded {
                preview.fade.enter(GROW);
            } else {
                preview.fade.exit(SHRINK);
            }
            cx.notify();
        });
    }
    if let Some(source) = cx.try_global::<Riding>().map(|riding| riding.source) {
        let _ = source.update(cx, |_, window, _| window.refresh());
    }
}

/// The drag ended: the preview goes, either way. A tear-out opens its
/// window where the preview stood.
fn finish(cx: &mut App) {
    if let Some(window) = cx.remove_global::<Riding>().window {
        close(window.into(), cx);
    }
}

fn close(window: AnyWindowHandle, cx: &mut App) {
    cx.defer(move |cx| {
        let _ = cx.update_window(window, |_, window, _| window.remove_window());
    });
}

/// Carry the preview with the pointer, and drop it on release, for a drag
/// that began in this window. Registered for every frame of every panel
/// window, during paint.
pub(crate) fn listen(window: &mut Window) {
    window.on_mouse_event(|_: &MouseMoveEvent, phase, window, cx| {
        if phase == DispatchPhase::Bubble {
            follow(window, cx);
        }
    });
    window.on_mouse_event(|event: &MouseUpEvent, phase, window, cx| {
        if phase == DispatchPhase::Bubble
            && event.button == MouseButton::Left
            && riding(window.window_handle(), cx)
        {
            finish(cx);
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
    let _ = preview.update(cx, |_, preview, _| native::move_by(preview, travel));
    cx.global_mut::<Riding>().cursor = Some(now);
}

/// The window a drag would tear out, drawn with its panes' live views.
struct DragOutPreview {
    title: SharedString,
    replica: Replica,
    /// Where the drag ghost sits in this window: the preview grows out of
    /// it and shrinks back into it.
    ghost: Bounds<Pixels>,
    fade: Fade,
}

impl Render for DragOutPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.fade.opacity(window, cx);
        let root = div()
            .when_some(
                cx.try_global::<crate::theme::FontSettings>()
                    .map(|font| font.family.clone()),
                |root, family| root.font_family(family),
            )
            .size_full()
            .relative();
        if t <= 0.0 {
            return root;
        }
        let theme = theme(cx);
        let full = window.viewport_size();
        let from = self.ghost;
        let lerp = |a: Pixels, b: Pixels| a + (b - a) * t;
        let left = lerp(from.origin.x, px(0.0));
        let top = lerp(from.origin.y, px(0.0));
        let width = lerp(from.size.width, full.width);
        let height = lerp(from.size.height, full.height);

        let titlebar = div()
            .h(px(crate::app::TITLEBAR_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .px(px(8.0))
            .bg(theme.bg_secondary)
            .border_b_1()
            .border_color(theme.border_primary)
            .child(
                div()
                    .h(px(CONTROL_HEIGHT))
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .max_w(px(crate::tiles::tab_bar::TAB_MAX_WIDTH))
                    .rounded(px(CONTROL_RADIUS))
                    .bg(theme.bg_primary)
                    .border_1()
                    .border_color(theme.border_primary)
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(div().truncate().child(self.title.clone())),
            );
        // Laid out at the full size throughout, and uncovered as the frame
        // grows, so the panes never reflow mid-animation.
        let content = div()
            .absolute()
            .left(-left)
            .top(-top)
            .w(full.width)
            .h(full.height)
            .flex()
            .flex_col()
            .bg(theme.bg_secondary)
            .opacity(t)
            .child(titlebar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(render_replica(&self.replica, cx)),
            );
        root.child(
            div()
                .absolute()
                .left(left)
                .top(top)
                .w(width)
                .h(height)
                .overflow_hidden()
                .rounded(lerp(px(CONTROL_RADIUS), px(10.0)))
                .border_1()
                .border_color(theme.border_primary)
                .bg(theme.bg_elevated)
                .shadow_lg()
                .child(content),
        )
    }
}

fn render_replica(replica: &Replica, cx: &App) -> gpui::AnyElement {
    let theme = theme(cx);
    match replica {
        Replica::Pane { title, view } => div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.bg_primary)
            .child(
                div()
                    .h(px(CONTROL_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(div().truncate().child(title.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .when_some(view.clone(), |slot, view| {
                        slot.child(div().absolute().size_full().child(view))
                    }),
            )
            .into_any_element(),
        Replica::Split { axis, parts } => {
            let mut split = div()
                .size_full()
                .flex()
                .gap(px(1.0))
                .bg(theme.border_primary);
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
                        .child(render_replica(part, cx));
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
    /// display), so a preview carried across displays never jumps.
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
