//! A dragged pane or tab rides the pointer in a window of its own: its tab,
//! drawn as the drag ghost, and — once the drag leaves its window — its
//! live panes unfolding below it. Nothing has moved yet: bring the pointer
//! back and the panes fold back up into the tab and the drag carries on;
//! release outside and the window root tears the content out for real,
//! its panes where the preview's were.
//!
//! gpui draws a drag ghost inside its source window only, clipped at its
//! edge, and can't place a window once it is open, so the preview — the
//! ghost included — is a window of its own placed through AppKit. The source window keeps receiving the dragged pointer
//! (AppKit sends a drag's events to the window the press began in), and
//! each move pins the preview to the pointer. Other platforms go without
//! the preview.
//!
//! The preview draws the panes' own views a second time, laid out small:
//! gpui has no transform for live elements and no readback of a window's
//! pixels outside tests. Views that render on the GPU at their own size
//! (plots, the 3D viewer) check [`is_preview`] and show their last frame
//! instead of rendering again at the preview's size — doing so would
//! reallocate their targets twice a frame, fighting the source window.
//!
//! The preview is moved behind gpui's back: with its window delegate
//! detached, AppKit doesn't report the moves, which gpui would otherwise
//! answer with a full redraw on every pointer move.

use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, DispatchPhase, Global, IntoElement,
    MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Size, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, point,
    prelude::*, px, relative, size,
};

use crate::motion::Fade;
use crate::theme::{CONTROL_HEIGHT, CONTROL_RADIUS, theme};
use crate::tiles::Replica;
use crate::tiles::drag::DragSource;

const GROW: Duration = Duration::from_millis(180);
const SHRINK: Duration = Duration::from_millis(120);
/// Width the preview scales the panes down to.
const MINI_WIDTH: f32 = 420.0;
/// How far the panes sit inside the preview's frame. gpui clips children
/// to rectangles, so this keeps their square corners within the frame's
/// rounded ones.
const INSET: f32 = 3.0;
/// The preview frame's rounding, and its border.
const FRAME_RADIUS: f32 = 8.0;
const BORDER: f32 = 1.0;
/// Room around the preview inside its window: for the shadow, and clear of
/// the corners AppKit rounds off every window.
const MARGIN: f32 = 16.0;

struct Riding {
    /// The window the drag began in, which is fed the pointer until release.
    source: AnyWindowHandle,
    window: Option<WindowHandle<DragOutPreview>>,
    /// Out of the source window, so the preview shows and the ghost hides.
    expanded: bool,
    /// Where the pointer holds the preview window, from its top-left.
    grab: Point<Pixels>,
    /// The preview window, known from its first frame on.
    preview: Option<AnyWindowHandle>,
}

impl Global for Riding {}

/// Whether this platform can carry a preview under the pointer.
pub(crate) fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// The size of the window a pane or tab tears out into: a pane keeps its
/// own size (with room for a titlebar), a tab takes its window's.
pub(crate) fn torn_size(source: &DragSource, window: &Window, cx: &App) -> Size<Pixels> {
    match source {
        DragSource::Pane(pane) => {
            let outer = pane.read(cx).current_outer_size();
            size(
                px(outer.width.max(400.0)),
                px(outer.height.max(300.0) + crate::app::TITLEBAR_HEIGHT),
            )
        }
        DragSource::Tab(..) => window.bounds().size,
    }
}

/// What a panel window's frame takes from its panes: the titlebar, and the
/// tab sidebar when tabs sit down the side.
pub(crate) fn window_chrome(cx: &App) -> Size<Pixels> {
    let sidebar = match crate::tiles::tab_bar::placement(cx) {
        crate::config::TabPlacement::Side => crate::tiles::tab_bar::sidebar_width(cx),
        crate::config::TabPlacement::Top => px(0.0),
    };
    size(sidebar, px(crate::app::TITLEBAR_HEIGHT))
}

/// The size the preview draws panes of `panes` size at: small enough to
/// carry, in the same proportions.
fn body_size(panes: Size<Pixels>) -> Size<Pixels> {
    let scale = (MINI_WIDTH / f32::from(panes.width)).min(1.0);
    size(panes.width * scale, panes.height * scale)
}

/// Where the preview's panes start, from the pointer: just below the
/// ghost the pointer holds `grab` into. A window torn out where the preview
/// stood puts its panes here too.
pub(crate) fn panes_offset(grab: Point<Pixels>) -> Point<Pixels> {
    // The frame tucks its top border under the tab's bottom one.
    point(px(BORDER + INSET), px(CONTROL_HEIGHT + INSET)) - grab
}

/// The frame around panes of `body` size: border and inset on every side.
fn frame_size(body: Size<Pixels>) -> Size<Pixels> {
    let edge = px(BORDER + INSET) * 2.0;
    size(body.width + edge, body.height + edge)
}

/// Whether `window` is a drag preview, where views show what they last
/// rendered rather than render again.
pub(crate) fn is_preview(window: &Window, cx: &App) -> bool {
    cx.try_global::<Riding>()
        .is_some_and(|riding| riding.preview == Some(window.window_handle()))
}

/// Whether a drag from `source` has its panes out, below the tab.
pub(crate) fn expanded(source: AnyWindowHandle, cx: &App) -> bool {
    cx.try_global::<Riding>()
        .is_some_and(|riding| riding.source == source && riding.expanded)
}

/// Whether a drag from `source` rides in a preview window, which stands in
/// for gpui's ghost.
pub(crate) fn riding(source: AnyWindowHandle, cx: &App) -> bool {
    cx.try_global::<Riding>()
        .is_some_and(|riding| riding.source == source)
}

/// A pane or tab drag just started in `window`: open its preview, folded
/// up to the tab — `title`, `width` wide when the tab's width is known —
/// under the pointer. Reads the drag from the [`ActiveDrag`] mirror, so
/// call it once that is set.
///
/// [`ActiveDrag`]: crate::tiles::drag::ActiveDrag
pub(crate) fn begin(title: SharedString, width: Option<Pixels>, window: &Window, cx: &mut App) {
    if !supported() {
        return;
    }
    let Some(drag) = crate::tiles::drag::active_drag(cx) else {
        return;
    };
    let replica = match &drag.source {
        DragSource::Pane(pane) => Some(Replica::titled_pane(pane, cx).1),
        DragSource::Tab(group, id) => group.read(cx).tab_replica(*id, cx).map(|(_, r)| r),
    };
    let Some(replica) = replica else {
        return;
    };
    let body = body_size(torn_size(&drag.source, window, cx) - window_chrome(cx));
    let closable = drag.closable;
    let source = window.window_handle();
    let margin = point(px(MARGIN), px(MARGIN));
    let grab = drag.grab + margin;
    let frame = frame_size(body);
    let extent = size(
        frame.width.max(width.unwrap_or_default()) + px(MARGIN) * 2.0,
        px(CONTROL_HEIGHT - BORDER) + frame.height + px(MARGIN) * 2.0,
    );
    let bounds = Bounds {
        origin: window.bounds().origin + window.mouse_position() - grab,
        size: extent,
    };
    cx.set_global(Riding {
        source,
        window: None,
        expanded: false,
        grab,
        preview: None,
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
                if cx.has_global::<Riding>() {
                    cx.global_mut::<Riding>().preview = Some(window.window_handle());
                }
                native::detach(window);
                native::pin(window, grab);
                cx.new(|_| DragOutPreview {
                    title,
                    closable,
                    tab: width,
                    body,
                    replica,
                    fade: Fade::settled(0.0),
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
                if expanded {
                    set_expanded(true, cx);
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
    /// The tab's width, when the bar measured it; otherwise it fits its
    /// title, as the ghost does.
    tab: Option<Pixels>,
    body: Size<Pixels>,
    replica: Replica,
    fade: Fade,
}

impl Render for DragOutPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.fade.opacity(window, cx);
        let theme = theme(cx);
        let margin = px(MARGIN);
        let frame = frame_size(self.body);
        let lerp = |a: Pixels, b: Pixels| a + (b - a) * t;
        // The tab widens with the panes until it heads the whole preview.
        let folded = self
            .tab
            .unwrap_or_else(|| folded_width(&self.title, self.closable))
            .min(frame.width);
        let mut tab = crate::tiles::drag::ghost_chip(self.title.clone(), self.closable, cx)
            .absolute()
            .left(margin)
            .top(margin);
        tab = if t > 0.0 {
            tab.w(lerp(folded, frame.width))
                .max_w(frame.width)
                .rounded_t(lerp(px(CONTROL_RADIUS), px(FRAME_RADIUS)))
                .rounded_b(lerp(px(CONTROL_RADIUS), px(0.0)))
        } else {
            tab.when_some(self.tab, |tab, width| tab.w(width))
        };
        // Unfolds downward from under the tab, laid out at its full size
        // throughout and uncovered as the frame grows, so the panes never
        // reflow mid-animation.
        let body = (t > 0.0).then(|| {
            div()
                .absolute()
                .left(margin)
                .top(margin + px(CONTROL_HEIGHT - BORDER))
                .w(lerp(folded, frame.width))
                .h(lerp(px(0.0), frame.height))
                .overflow_hidden()
                .rounded_b(px(FRAME_RADIUS))
                .border_1()
                .border_color(theme.border_primary)
                .bg(theme.bg_secondary)
                .shadow_lg()
                .child(
                    div()
                        .absolute()
                        .left(px(INSET))
                        .top(px(INSET))
                        .w(self.body.width)
                        .h(self.body.height)
                        .flex()
                        .opacity(t)
                        // A lone pane goes without its header: the tab
                        // above already names it.
                        .child(render_replica(
                            &self.replica,
                            matches!(self.replica, Replica::Split { .. }),
                            cx,
                        )),
                )
        });
        div()
            .when_some(
                cx.try_global::<crate::theme::FontSettings>()
                    .map(|font| font.family.clone()),
                |root, family| root.font_family(family),
            )
            .size_full()
            .relative()
            .children(body)
            .child(tab)
    }
}

/// About how wide a ghost that fits its title draws, for a pane ghost whose
/// width no bar measured: what the tab widens from as the panes unfold.
fn folded_width(title: &str, closable: bool) -> Pixels {
    let text = title.chars().count() as f32 * 7.0;
    let chrome = if closable { 36.0 } else { 18.0 };
    px((text + chrome).clamp(80.0, crate::tiles::tab_bar::TAB_MAX_WIDTH))
}

fn render_replica(replica: &Replica, headed: bool, cx: &App) -> gpui::AnyElement {
    let theme = theme(cx);
    match replica {
        Replica::Pane { title, view } => div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.bg_primary)
            .when(headed, |pane| {
                pane.child(
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
            })
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
                        .child(render_replica(part, true, cx));
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

    /// Make `window` a pure overlay: clicks and the pointer pass through
    /// it, it has no outline of its own, and moving it goes unreported.
    pub fn detach(window: &Window) {
        if let Some(ns_window) = ns_window(window) {
            ns_window.setIgnoresMouseEvents(true);
            // The frame draws its own shadow; AppKit's would trace the
            // whole window, visible or not.
            ns_window.setHasShadow(false);
            // gpui's delegate answers every move with a full redraw. The
            // window never resizes, takes focus or closes itself, and gpui
            // detaches the delegate on close anyway.
            ns_window.setDelegate(None);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use gpui::{Pixels, Point, Window};

    pub fn pin(_: &Window, _: Point<Pixels>) {}

    pub fn detach(_: &Window) {}
}
