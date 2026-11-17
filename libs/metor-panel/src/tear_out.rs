//! While a pane or tab is dragged outside its window, its live panes ride
//! the pointer, hanging from the dragged tab. Nothing has moved yet: bring
//! the pointer back and the panes fold back up into the tab and the drag
//! carries on; release outside and the window root tears the content out
//! for real, its panes where the preview's were.
//!
//! gpui draws a drag ghost inside its source window only and can't place a
//! window once it is open, so the preview is a window of its own placed
//! through AppKit. The source window keeps receiving the dragged pointer
//! (AppKit sends a drag's events to the window the press began in), and
//! each move pins the preview to the pointer. Other platforms go without
//! the preview.
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
use crate::theme::{CONTROL_HEIGHT, theme};
use crate::tiles::Replica;

const GROW: Duration = Duration::from_millis(180);
const SHRINK: Duration = Duration::from_millis(120);
/// Width the mini preview scales the panes down to.
const MINI_WIDTH: f32 = 420.0;
/// Room around the preview's frame for its shadow, inside the window.
const MARGIN: f32 = 16.0;

struct Riding {
    /// The window the drag began in, which is fed the pointer until release.
    source: AnyWindowHandle,
    window: Option<WindowHandle<DragOutPreview>>,
    /// Out of the source window, so the preview shows and the ghost hides.
    expanded: bool,
    /// Where the pointer holds the preview window, from its top-left.
    grab: Point<Pixels>,
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

/// The size the preview draws panes of `panes` size at.
fn body_size(panes: Size<Pixels>, cx: &App) -> Size<Pixels> {
    match preference(cx) {
        DragPreview::Full => panes,
        DragPreview::Mini => {
            let scale = (MINI_WIDTH / f32::from(panes.width)).min(1.0);
            size(panes.width * scale, panes.height * scale)
        }
    }
}

/// Where the preview's panes start, from the pointer: just below the
/// ghost the pointer holds `grab` into. A window torn out where the preview
/// stood puts its panes here too.
pub(crate) fn panes_offset(grab: Point<Pixels>) -> Point<Pixels> {
    point(px(0.0), px(CONTROL_HEIGHT)) - grab
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

/// What a preview shows: the dragged tab as its ghost showed it, and below
/// it the panes it carries, laid out at `panes` size.
pub(crate) struct Spec {
    pub title: SharedString,
    pub closable: bool,
    /// The ghost's size, held at `grab` by the pointer.
    pub tab: Size<Pixels>,
    pub grab: Point<Pixels>,
    pub panes: Size<Pixels>,
    pub replica: Replica,
}

/// Open the preview for a drag from `source`, the pointer at `pointer`
/// (screen coordinates), growing out of its ghost.
pub(crate) fn open(source: AnyWindowHandle, pointer: Point<Pixels>, spec: Spec, cx: &mut App) {
    let body = body_size(spec.panes, cx);
    let margin = px(MARGIN);
    // The window's top-left corner is the tab's, so the tab sits where the
    // ghost did; the margin runs right and down, where the shadow falls.
    let extent = size(
        body.width.max(spec.tab.width) + margin,
        px(CONTROL_HEIGHT) + body.height + margin,
    );
    let grab = spec.grab;
    cx.set_global(Riding {
        source,
        window: None,
        expanded: true,
        grab,
    });
    let bounds = Bounds {
        origin: pointer - grab,
        size: extent,
    };
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
                native::pin(window, grab);
                cx.new(|_| DragOutPreview {
                    title: spec.title,
                    closable: spec.closable,
                    tab: spec.tab,
                    body,
                    replica: spec.replica,
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

/// Grow the panes back out of the tab, or fold them up into it.
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
    let Some((preview, grab)) = cx
        .try_global::<Riding>()
        .filter(|riding| riding.source == this)
        .and_then(|riding| Some((riding.window?, riding.grab)))
    else {
        return;
    };
    let _ = preview.update(cx, |_, preview, _| native::pin(preview, grab));
}

/// The dragged tab, drawn as its ghost, with the panes it carries hanging
/// below it.
struct DragOutPreview {
    title: SharedString,
    closable: bool,
    tab: Size<Pixels>,
    body: Size<Pixels>,
    replica: Replica,
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
        let lerp = |a: Pixels, b: Pixels| a + (b - a) * t;
        // Unfolds downward from under the tab, laid out at its full size
        // throughout and uncovered as the frame grows, so the panes never
        // reflow mid-animation.
        let body = div()
            .absolute()
            .left_0()
            .top(px(CONTROL_HEIGHT - 1.0))
            .w(lerp(self.tab.width, self.body.width))
            .h(lerp(px(0.0), self.body.height))
            .overflow_hidden()
            .rounded_b(px(8.0))
            .rounded_tr(px(8.0))
            .border_1()
            .border_color(theme.border_primary)
            .bg(theme.bg_secondary)
            .shadow_lg()
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .w(self.body.width)
                    .h(self.body.height)
                    .flex()
                    .opacity(t)
                    .child(render_replica(&self.replica, cx)),
            );
        let tab = crate::tiles::drag::ghost_chip(self.title.clone(), self.closable, cx)
            .absolute()
            .left_0()
            .top_0()
            .w(self.tab.width);
        root.child(body).child(tab)
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
    use gpui::{Pixels, Point, Window};
    use objc2_app_kit::{NSEvent, NSView, NSWindow};
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

    /// Let clicks and the pointer pass through to whatever lies beneath,
    /// and leave the window no outline of its own.
    pub fn ignore_mouse(window: &Window) {
        if let Some(ns_window) = ns_window(window) {
            ns_window.setIgnoresMouseEvents(true);
            // The frame draws its own shadow; AppKit's would trace the
            // whole window, visible or not.
            ns_window.setHasShadow(false);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use gpui::{Pixels, Point, Window};

    pub fn pin(_: &Window, _: Point<Pixels>) {}

    pub fn ignore_mouse(_: &Window) {}
}
