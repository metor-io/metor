//! Carries a pane or tab drag into the other panel windows.
//!
//! The platform sends every event of a drag to the window it began in, so
//! another window never sees the pointer cross it. The source window
//! forwards each move to the panel window under the pointer, which then
//! previews the drop as it would its own drag, and forwards the release, so
//! that window's own drop handlers land the tab or pane. gpui's drag is
//! app-wide, so the forwarded events find it already in flight.

use gpui::{
    AnyWindowHandle, App, AppContext, DispatchPhase, Global, Modifiers, MouseButton,
    MouseMoveEvent, MouseUpEvent, PlatformInput, Point, Window, point, px,
};

use crate::app::AppRoot;

/// The drag being relayed: the window it began in, and the window the
/// pointer is over.
struct Relay {
    source: AnyWindowHandle,
    target: Option<AnyWindowHandle>,
}

impl Global for Relay {}

/// Forward this window's dragged pointer to the panel window under it.
/// Registered for every frame of every panel window, during paint.
pub(crate) fn listen(window: &mut Window) {
    window.on_mouse_event(|event: &MouseMoveEvent, phase, window, cx| {
        if phase == DispatchPhase::Capture {
            follow(event, window, cx);
        }
    });
}

fn follow(event: &MouseMoveEvent, window: &mut Window, cx: &mut App) {
    let this = window.window_handle();
    if !cx.has_active_drag() {
        if cx
            .try_global::<Relay>()
            .is_some_and(|relay| relay.source == this)
        {
            end(None, cx);
        }
        return;
    }
    let source = match cx.try_global::<Relay>() {
        Some(relay) => relay.source,
        None => match crate::tiles::drag::active_drag(cx) {
            Some(drag) => drag.source_window,
            None => return,
        },
    };
    if source != this {
        return;
    }
    let screen = window.bounds().origin + event.position;
    let target = window_under(screen, window, cx);
    let previous = cx
        .try_global::<Relay>()
        .and_then(|relay| relay.target)
        .filter(|previous| Some(*previous) != target.map(|(handle, _)| handle));
    if let Some(previous) = previous {
        leave(previous, cx);
    }
    cx.set_global(Relay {
        source,
        target: target.map(|(handle, _)| handle),
    });
    if let Some((target, local)) = target {
        forward(
            target,
            PlatformInput::MouseMove(MouseMoveEvent {
                position: local,
                pressed_button: Some(MouseButton::Left),
                modifiers: event.modifiers,
            }),
            cx,
        );
    }
}

/// Dispatch `input` in `target`, in the midst of this window's own
/// dispatch. Propagation is app-wide in gpui: a handler over there that
/// stops it would otherwise stop this window's listeners too, such as the
/// chip's.
fn forward(target: AnyWindowHandle, input: PlatformInput, cx: &mut App) {
    let _ = cx.update_window(target, |_, window, cx| {
        window.dispatch_event(input, cx);
        window.refresh();
    });
    cx.propagate();
}

/// The drag was released in `window` at `position`: when that was over
/// another panel window, drop it there and return true. Call before this
/// window acts on the release itself, while gpui's drag is still in flight.
pub(crate) fn release(position: Point<gpui::Pixels>, window: &mut Window, cx: &mut App) -> bool {
    let this = window.window_handle();
    if !cx
        .try_global::<Relay>()
        .is_some_and(|relay| relay.source == this)
    {
        return false;
    }
    let target = cx
        .has_active_drag()
        .then(|| window_under(window.bounds().origin + position, window, cx))
        .flatten();
    end(target.map(|(handle, _)| handle), cx);
    let Some((target, local)) = target else {
        return false;
    };
    forward(
        target,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: local,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
        cx,
    );
    let _ = cx.update_window(target, |_, window, _| window.activate_window());
    // The target ends gpui's drag as it handles the release; one it didn't
    // take ends here, so the source can't land it as well.
    cx.stop_active_drag(window);
    crate::drag_chip::finish(cx);
    crate::app::close_if_emptied(this, cx);
    true
}

/// Stop relaying, leaving every window but `keep` as if the pointer never
/// crossed it.
fn end(keep: Option<AnyWindowHandle>, cx: &mut App) {
    let Some(relay) = cx
        .has_global::<Relay>()
        .then(|| cx.remove_global::<Relay>())
    else {
        return;
    };
    if let Some(target) = relay.target.filter(|target| Some(*target) != keep) {
        leave(target, cx);
    }
}

/// The pointer left `target`: move it far off, so its hovers and drop
/// previews clear.
fn leave(target: AnyWindowHandle, cx: &mut App) {
    forward(
        target,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: point(px(-100_000.0), px(-100_000.0)),
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
        cx,
    );
}

/// The frontmost panel window other than `source` at screen point
/// `screen`, with the point in its own coordinates. None when `source`
/// itself is in front there.
fn window_under(
    screen: Point<gpui::Pixels>,
    source: &Window,
    cx: &mut App,
) -> Option<(AnyWindowHandle, Point<gpui::Pixels>)> {
    let this = source.window_handle();
    let stack = cx.window_stack().unwrap_or_else(|| cx.windows());
    for handle in stack {
        if handle == this {
            if source.bounds().contains(&screen) {
                return None;
            }
            continue;
        }
        if handle.downcast::<AppRoot>().is_none() {
            continue;
        }
        let Ok(bounds) = cx.update_window(handle, |_, window, _| window.bounds()) else {
            continue;
        };
        if bounds.contains(&screen) {
            return Some((handle, screen - bounds.origin));
        }
    }
    None
}
