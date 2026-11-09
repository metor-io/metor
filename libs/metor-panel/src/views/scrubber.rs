//! The panel's one scrubber: a [`ValueBox`] that is also a slider. Dragging
//! anywhere in it moves the value by how far the pointer travels, a click
//! asks to type one, and a fill and a thin handle show where the value sits
//! in its range. Like [`Switch`](super::switch::Switch) it is sized by its
//! height, so other hosts can draw the same control at their own sizes.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Context, CursorStyle, DragMoveEvent, ElementId, Empty, Pixels, Render,
    Stateful, Window, canvas, div, fill, prelude::*, px, relative, size,
};

use crate::theme::Theme;
use crate::views::value_box::{HOVER_GROUP, OnEdit, ValueBox};
use crate::views::value_strip::BOX_RADIUS;

/// Called with each new value while the scrubber is dragged.
pub(crate) type OnScrub = Rc<dyn Fn(f64, &mut Window, &mut App)>;

/// How a scrubber looks and what it reports; the caller owns the value, and
/// styles its own element with [`Scrubber::apply`].
pub(crate) struct Scrubber {
    id: ElementId,
    value: f64,
    min: f64,
    max: f64,
    step: f64,
    height: f32,
    text_size: Option<f32>,
    align_end: bool,
    on_change: Option<OnScrub>,
    on_edit: Option<OnEdit>,
    editing: Option<AnyElement>,
}

impl Scrubber {
    /// A scrubber over `min..=max`, `height` pixels tall. `id` keys the
    /// drag, so it must be unique among scrubbers on screen. The step
    /// defaults to the power of ten nearest a hundredth of the range.
    pub(crate) fn new(
        id: impl Into<ElementId>,
        value: f64,
        min: f64,
        max: f64,
        height: f32,
    ) -> Self {
        Self {
            id: id.into(),
            value,
            min,
            max,
            step: default_step(min, max),
            height,
            text_size: None,
            align_end: false,
            on_change: None,
            on_edit: None,
            editing: None,
        }
    }

    /// Snap values to multiples of `step`; a shift-drag snaps to a tenth of
    /// it.
    pub(crate) fn step(mut self, step: f64) -> Self {
        if step > 0.0 && step.is_finite() {
            self.step = step;
        }
        self
    }

    /// The value's text size, where the host's text is a fixed size rather
    /// than the switch's 11/24 of the height.
    pub(crate) fn text_size(mut self, size: f32) -> Self {
        self.text_size = Some(size);
        self
    }

    /// Let a drag change the value. Without it the scrubber only shows the
    /// value, at full strength.
    pub(crate) fn on_change(mut self, change: OnScrub) -> Self {
        self.on_change = Some(change);
        self
    }

    /// Let a click ask to type a value.
    pub(crate) fn on_edit(mut self, edit: OnEdit) -> Self {
        self.on_edit = Some(edit);
        self
    }

    /// Set the value against the right edge, where it lines up with other
    /// values in a column (an inspector's), rather than centred.
    pub(crate) fn align_end(mut self) -> Self {
        self.align_end = true;
        self
    }

    /// Show `field` inside the box in place of the value while the caller
    /// edits it as text; dragging is off meanwhile.
    pub(crate) fn editing(mut self, field: impl IntoElement) -> Self {
        self.editing = Some(field.into_any_element());
        self
    }

    /// Style `el` as this scrubber: a [`ValueBox`] with a fill and a
    /// handle, which a drag across moves the value.
    pub(crate) fn apply(self, el: gpui::Div, theme: &Theme) -> Stateful<gpui::Div> {
        let h = self.height;
        let ranged = self.max > self.min;
        let fraction = match ranged {
            true => ((self.value - self.min) / (self.max - self.min)).clamp(0.0, 1.0) as f32,
            false => 0.0,
        };
        let editing = self.editing.is_some();
        let scrubs = ranged && !editing && self.on_change.is_some();

        let mut value_box = ValueBox::new(self.id.clone(), format_value(self.value, self.step), h)
            .layer(fill_layer(fraction, theme.scrub_fill()));
        if scrubs {
            value_box = value_box.layer(handle_layer(fraction, h, theme));
        }
        if let Some(size) = self.text_size {
            value_box = value_box.text_size(size);
        }
        if self.align_end {
            value_box = value_box.align_end();
        }
        if let Some(edit) = self.on_edit.clone() {
            value_box = value_box.on_edit(edit);
        }
        if let Some(field) = self.editing {
            value_box = value_box.editing(field);
        }
        let el = value_box
            .apply(el, theme)
            .debug_selector(|| "scrubber".into());

        let Some(on_change) = self.on_change.filter(|_| scrubs) else {
            return el;
        };
        let id = self.id;
        let (min, max, step, start) = (self.min, self.max, self.step, self.value);
        el.cursor(CursorStyle::ResizeLeftRight)
            .on_drag(
                Scrub {
                    id: id.clone(),
                    min,
                    max,
                    step,
                    anchor: Cell::new(Anchor {
                        x: 0.0,
                        value: start,
                        fine: false,
                    }),
                    last: Cell::new(start),
                    on_change,
                },
                |scrub, offset, _, cx| {
                    scrub.anchor.set(Anchor {
                        x: f32::from(offset.x),
                        ..scrub.anchor.get()
                    });
                    cx.new(|_| NoPreview)
                },
            )
            .on_drag_move(move |event: &DragMoveEvent<Scrub>, window, cx| {
                let scrub = event.drag(cx);
                // Every scrubber hears every scrub, and each measures
                // against its own box.
                if scrub.id != id {
                    return;
                }
                let x = f32::from(event.event.position.x - event.bounds.origin.x);
                let fine = event.event.modifiers.shift;
                let mut anchor = scrub.anchor.get();
                // Changing precision mid-drag continues from where the
                // value is, so the handle doesn't jump.
                if fine != anchor.fine {
                    anchor = Anchor {
                        x,
                        value: scrub.last.get(),
                        fine,
                    };
                    scrub.anchor.set(anchor);
                }
                let travel = travel(event.bounds);
                let value = scrubbed(
                    anchor.value,
                    x - anchor.x,
                    travel,
                    (scrub.min, scrub.max),
                    scrub.step,
                    fine,
                );
                if value != scrub.last.get() {
                    scrub.last.set(value);
                    let change = scrub.on_change.clone();
                    change(value, window, cx);
                }
            })
    }
}

/// The filled part of the box, painted from its left edge to the value's
/// place across the whole width.
fn fill_layer(fraction: f32, color: gpui::Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let width = bounds.size.width * fraction;
            if width <= px(0.0) {
                return;
            }
            // The box's rounding doesn't clip what is painted in it, so the
            // fill carries the box's left corners, and its right ones as it
            // reaches the far side.
            let radius = px(BOX_RADIUS);
            let right = (radius - (bounds.size.width - width)).max(px(0.0));
            let mut quad = fill(
                Bounds::new(bounds.origin, size(width, bounds.size.height)),
                color,
            );
            quad.corner_radii = gpui::Corners {
                top_left: radius,
                bottom_left: radius,
                top_right: right,
                bottom_right: right,
            };
            window.paint_quad(quad);
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The thin handle at the value's place, shaped like a switch's knob and
/// shown while the box is hovered.
fn handle_layer(fraction: f32, h: f32, theme: &Theme) -> impl IntoElement {
    let inset = h / 7.0;
    let handle_w = h / 5.0;
    let handle = div()
        .debug_selector(|| "scrubber-handle".into())
        .absolute()
        .top_0()
        .bottom_0()
        .left(relative(fraction))
        .w(px(handle_w))
        .rounded(px(h / 7.0))
        .bg(theme.text_primary)
        .opacity(0.0)
        .group_hover(HOVER_GROUP, |s| s.opacity(1.0));
    div()
        .absolute()
        .top(px(inset))
        .bottom(px(inset))
        .left(px(inset))
        .right(px(inset + handle_w))
        .child(handle)
}

/// Where a drag measures from: the pointer and the value when it began, or
/// when shift last changed.
#[derive(Clone, Copy)]
struct Anchor {
    x: f32,
    value: f64,
    fine: bool,
}

/// The drag's payload. The value at the start of the drag comes from the
/// frame it began in; later frames are only told how far the pointer went.
struct Scrub {
    id: ElementId,
    min: f64,
    max: f64,
    step: f64,
    anchor: Cell<Anchor>,
    last: Cell<f64>,
    on_change: OnScrub,
}

/// A scrub draws nothing under the pointer; the box itself shows it.
struct NoPreview;

impl Render for NoPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// How far the handle travels across a box, in the box's own pixels: its
/// width less the inset either side and the handle itself. A drag across
/// it covers the whole range, so the handle keeps up with the pointer. The
/// fill spans the whole box instead, empty at the bottom of the range and
/// full at the top, and meets the handle's centre midway.
fn travel(bounds: Bounds<Pixels>) -> f32 {
    let h = f32::from(bounds.size.height);
    (f32::from(bounds.size.width) - 2.0 * h / 7.0 - h / 5.0).max(1.0)
}

/// The value a drag of `dx` pixels from `start` reaches: the whole range
/// across the handle's travel, a tenth as fast with `fine`, snapped and
/// clamped.
fn scrubbed(
    start: f64,
    dx: f32,
    travel: f32,
    (min, max): (f64, f64),
    step: f64,
    fine: bool,
) -> f64 {
    let scale = if fine { 0.1 } else { 1.0 };
    let value = start + (dx / travel) as f64 * (max - min) * scale;
    let step = if fine { step / 10.0 } else { step };
    snap(value, step).clamp(min, max)
}

/// The value one notch on from `value` in `direction`, a tenth of a step
/// with `fine`: what an arrow key does to a scrubber.
pub(crate) fn nudged(
    value: f64,
    (min, max): (f64, f64),
    step: f64,
    direction: f64,
    fine: bool,
) -> f64 {
    let step = if fine { step / 10.0 } else { step };
    snap(value + direction.signum() * step, step).clamp(min, max)
}

fn snap(value: f64, step: f64) -> f64 {
    let snapped = (value / step).round() * step;
    // Drop the float noise a multiple of 0.1 picks up, so the value sent
    // is the one shown.
    let scale = 10f64.powi(decimals(step) as i32);
    (snapped * scale).round() / scale
}

/// The power of ten nearest a hundredth of the range: 0.01 across `0..1`,
/// 10 across `0..1000`.
pub(crate) fn default_step(min: f64, max: f64) -> f64 {
    let range = max - min;
    if !(range.is_finite() && range > 0.0) {
        return 1.0;
    }
    10f64.powf((range / 100.0).log10().floor())
}

/// Decimal places a step needs to show.
fn decimals(step: f64) -> usize {
    (-step.log10().floor()).clamp(0.0, 9.0) as usize
}

/// The value to the step's places, or one more for a value a fine drag left
/// between steps.
pub(crate) fn format_value(value: f64, step: f64) -> String {
    let on_step = ((value / step) - (value / step).round()).abs() < 1e-6;
    let places = decimals(step) + usize::from(!on_step);
    format!("{value:.places$}")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{Context, Modifiers, MouseButton, Window, div, point, px};

    use super::*;

    #[test]
    fn the_default_step_is_a_hundredth_of_the_range() {
        assert_eq!(default_step(0.0, 1.0), 0.01);
        assert_eq!(default_step(0.0, 1000.0), 10.0);
        assert_eq!(default_step(-50.0, 50.0), 1.0);
        assert_eq!(default_step(1.0, 1.0), 1.0, "an empty range still steps");
    }

    #[test]
    fn a_drag_moves_by_distance_not_to_the_pointer() {
        let range = (0.0, 1.0);
        assert_eq!(scrubbed(0.5, 50.0, 100.0, range, 0.01, false), 1.0);
        assert_eq!(scrubbed(0.5, -25.0, 100.0, range, 0.01, false), 0.25);
        assert_eq!(scrubbed(0.5, 50.0, 100.0, range, 0.01, true), 0.55);
        assert_eq!(
            scrubbed(0.9, 50.0, 100.0, range, 0.01, false),
            1.0,
            "clamped"
        );
    }

    #[test]
    fn a_nudge_steps_one_notch() {
        let range = (0.0, 1.0);
        assert_eq!(nudged(0.5, range, 0.01, 1.0, false), 0.51);
        assert_eq!(nudged(0.5, range, 0.01, -1.0, true), 0.499);
        assert_eq!(nudged(1.0, range, 0.01, 1.0, false), 1.0, "clamped");
    }

    #[test]
    fn values_snap_without_float_noise() {
        assert_eq!(snap(0.30000000000000004, 0.1), 0.3);
        assert_eq!(snap(0.123, 0.01), 0.12);
        assert_eq!(format_value(0.3, 0.01), "0.30");
        assert_eq!(
            format_value(0.125, 0.01),
            "0.125",
            "a fine value shows its place"
        );
        assert_eq!(format_value(40.0, 10.0), "40");
    }

    struct Rig {
        value: f64,
        edits: Rc<Cell<usize>>,
    }

    impl Render for Rig {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::theme(cx);
            let rig = cx.entity().downgrade();
            let edits = self.edits.clone();
            let scrubber = Scrubber::new("scrubber", self.value, 0.0, 1.0, 20.0)
                .on_change(Rc::new(move |value, _, cx| {
                    let _ = rig.update(cx, |rig, cx| {
                        rig.value = value;
                        cx.notify();
                    });
                }))
                .on_edit(Rc::new(move |_, _| edits.set(edits.get() + 1)));
            div()
                .flex()
                .child(scrubber.apply(div().w(px(120.0)), &theme))
        }
    }

    fn rig(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::Entity<Rig>,
        Rc<Cell<usize>>,
        &mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()))
        });
        let edits = Rc::new(Cell::new(0));
        let (rig, cx) = cx.add_window_view({
            let edits = edits.clone();
            move |_, _| Rig { value: 0.5, edits }
        });
        cx.update(|window, _| window.refresh());
        (rig, edits, cx)
    }

    #[gpui::test]
    fn dragging_scrubs_relative_to_where_it_started(cx: &mut gpui::TestAppContext) {
        let (rig, edits, cx) = rig(cx);
        // Pressing near the left edge doesn't jump the value there.
        let start = point(px(10.0), px(10.0));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            start + point(px(4.0), px(0.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        let moved = cx.read(|cx| rig.read(cx).value);
        assert!((moved - 0.5).abs() < 0.02, "still near the start: {moved}");

        // The lane is 120 - 2·20/7 - 4 ≈ 110px, so 55px more is about half
        // the range.
        cx.simulate_mouse_move(
            start + point(px(59.0), px(0.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            start + point(px(59.0), px(0.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        let value = cx.read(|cx| rig.read(cx).value);
        assert!((value - 1.0).abs() < 0.02, "about a full half on: {value}");
        assert_eq!(edits.get(), 0, "a drag isn't a click");
    }

    #[gpui::test]
    fn a_click_asks_to_type_a_value(cx: &mut gpui::TestAppContext) {
        let (rig, edits, cx) = rig(cx);
        cx.simulate_click(point(px(60.0), px(10.0)), Modifiers::default());
        assert_eq!(edits.get(), 1);
        assert_eq!(cx.read(|cx| rig.read(cx).value), 0.5);
    }
}
