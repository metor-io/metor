//! A value box that is also a slider. Dragging anywhere in the box moves
//! the value by how far the pointer travels (ten times slower with Shift),
//! and a fill and a hover handle show where the value sits in its range.

use std::cell::Cell;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, CursorStyle, DragMoveEvent, Empty, Pixels, Point, Render, Stateful,
    Window, canvas, div, fill, prelude::*, px, relative, size,
};

use crate::theme::Theme;
use crate::views::value_box::{HEIGHT, HOVER_GROUP};
use crate::views::value_strip::BOX_RADIUS;

/// Called with each new value while the scrubber is dragged.
pub(crate) type OnScrub = Arc<dyn Fn(f64, &mut Window, &mut App)>;

/// Inset of the handle's track from the box's edges.
const INSET: f32 = HEIGHT / 7.0;
const HANDLE_WIDTH: f32 = HEIGHT / 5.0;

/// A value in `min..=max` and how a drag or an arrow key changes it.
#[derive(Clone)]
pub(crate) struct Scrubber {
    value: f64,
    min: f64,
    max: f64,
    integer: bool,
    on_change: Option<OnScrub>,
}

impl Scrubber {
    /// With `integer` every value stays whole: the step is at least one and
    /// Shift doesn't make it finer. Without `on_change` the box only shows
    /// where the value sits.
    pub(crate) fn new(
        value: f64,
        (min, max): (f64, f64),
        integer: bool,
        on_change: Option<OnScrub>,
    ) -> Self {
        Self {
            value,
            min,
            max,
            integer,
            on_change,
        }
    }

    /// Whether the range is finite and neither empty nor inverted.
    fn ranged(&self) -> bool {
        self.max > self.min && (self.max - self.min).is_finite()
    }

    /// Whether a drag can change the value.
    pub(crate) fn scrubs(&self) -> bool {
        self.ranged() && self.on_change.is_some()
    }

    /// The power of ten nearest a hundredth of the range (0.01 across
    /// `0..1`, 10 across `0..1000`), and at least one for an integer.
    pub(crate) fn step(&self) -> f64 {
        let step = match self.ranged() {
            true => 10f64.powf(((self.max - self.min) / 100.0).log10().floor()),
            false => 1.0,
        };
        if self.integer { step.max(1.0) } else { step }
    }

    /// The step, or a tenth of it when `fine` and the value isn't an integer.
    fn step_for(&self, fine: bool) -> f64 {
        match fine && !self.integer {
            true => self.step() / 10.0,
            false => self.step(),
        }
    }

    /// The value as text, to the step's decimal places.
    pub(crate) fn text(&self) -> String {
        format_value(self.value, self.step())
    }

    /// Where a drag or an arrow key starts from: the value, or `min` when
    /// the value isn't a finite number.
    fn start(&self) -> f64 {
        if self.value.is_finite() {
            self.value
        } else {
            self.min
        }
    }

    /// `value` snapped to `step` and kept in the range. `f64::clamp` would
    /// panic on an inverted or NaN range; `max` then `min` doesn't.
    fn settle(&self, value: f64, step: f64) -> f64 {
        snap(value, step).max(self.min).min(self.max)
    }

    /// The value one step from the current one, up for a positive
    /// `direction`: what an arrow key does.
    pub(crate) fn nudged(&self, direction: f64, fine: bool) -> f64 {
        let step = self.step_for(fine);
        self.settle(self.start() + direction.signum() * step, step)
    }

    /// The value a drag of `dx` pixels reaches from `start`, where `travel`
    /// pixels cover the whole range, or a tenth of it when `fine`.
    /// An integer moves slower with `fine` but still in whole steps.
    fn scrubbed(&self, start: f64, dx: f32, travel: f32, fine: bool) -> f64 {
        let scale = if fine { 0.1 } else { 1.0 };
        let value = start + (dx / travel) as f64 * (self.max - self.min) * scale;
        self.settle(value, self.step_for(fine))
    }

    /// Where the value sits in the range, from 0 to 1.
    fn fraction(&self) -> f32 {
        match self.ranged() {
            true => ((self.start() - self.min) / (self.max - self.min)).clamp(0.0, 1.0) as f32,
            false => 0.0,
        }
    }

    /// The box's fill, from its left edge to the value's fraction of its
    /// width.
    pub(crate) fn fill(&self, theme: &Theme) -> impl IntoElement + use<> {
        let (fraction, color) = (self.fraction(), theme.scrub_fill());
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let width = bounds.size.width * fraction;
                if width <= px(0.0) {
                    return;
                }
                // The box's rounded corners don't clip what is painted in
                // it, so the fill rounds its own left corners, and its
                // right ones as it reaches the far side.
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

    /// A thin handle at the value's place, shown while the box is hovered.
    pub(crate) fn handle(&self, theme: &Theme) -> impl IntoElement + use<> {
        let handle = div()
            .debug_selector(|| "scrubber-handle".into())
            .absolute()
            .top_0()
            .bottom_0()
            .left(relative(self.fraction()))
            .w(px(HANDLE_WIDTH))
            .rounded(px(INSET))
            .bg(theme.text_primary)
            .opacity(0.0)
            .group_hover(HOVER_GROUP, |s| s.opacity(1.0));
        div()
            .absolute()
            .top(px(INSET))
            .bottom(px(INSET))
            .left(px(INSET))
            .right(px(INSET + HANDLE_WIDTH))
            .child(handle)
    }

    /// Make a drag across `el` scrub the value.
    pub(crate) fn attach(self, el: Stateful<gpui::Div>) -> Stateful<gpui::Div> {
        let Some(on_change) = self.on_change.clone() else {
            return el;
        };
        let start = self.start();
        el.cursor(CursorStyle::ResizeLeftRight)
            .on_drag(
                Scrub {
                    origin: Cell::new(Point::default()),
                    anchor: Cell::new(Anchor {
                        x: 0.0,
                        value: start,
                        fine: false,
                    }),
                    last: Cell::new(start),
                },
                |scrub, offset, window, cx| {
                    scrub.origin.set(window.mouse_position() - offset);
                    scrub.anchor.set(Anchor {
                        x: f32::from(offset.x),
                        fine: window.modifiers().shift,
                        ..scrub.anchor.get()
                    });
                    cx.new(|_| NoPreview)
                },
            )
            .on_drag_move(move |event: &DragMoveEvent<Scrub>, window, cx| {
                let scrub = event.drag(cx);
                // Every scrubber in the window hears every scrub; only the
                // box the drag began in acts on it.
                let moved = event.bounds.origin - scrub.origin.get();
                if f32::from(moved.x).abs() > 0.5 || f32::from(moved.y).abs() > 0.5 {
                    return;
                }
                let x = f32::from(event.event.position.x - event.bounds.origin.x);
                let fine = event.event.modifiers.shift;
                let mut anchor = scrub.anchor.get();
                // Pressing or releasing Shift continues from the current
                // value, so the handle doesn't jump.
                if fine != anchor.fine {
                    anchor = Anchor {
                        x,
                        value: scrub.last.get(),
                        fine,
                    };
                    scrub.anchor.set(anchor);
                }
                let value = self.scrubbed(anchor.value, x - anchor.x, travel(event.bounds), fine);
                if value != scrub.last.get() {
                    scrub.last.set(value);
                    on_change(value, window, cx);
                }
            })
    }
}

/// The pointer position and value a drag measures from: where it began, or
/// where Shift was last pressed or released.
#[derive(Clone, Copy)]
struct Anchor {
    x: f32,
    value: f64,
    fine: bool,
}

/// The drag's payload.
struct Scrub {
    /// Window position of the box the drag began in.
    origin: Cell<Point<Pixels>>,
    anchor: Cell<Anchor>,
    /// The value last sent to `on_change`.
    last: Cell<f64>,
}

/// A scrub draws nothing under the pointer; the box itself shows it.
struct NoPreview;

impl Render for NoPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// How far the handle can move across a box: its width less the insets
/// and the handle. Dragging this far covers the whole range, so the handle
/// keeps up with the pointer.
fn travel(bounds: Bounds<Pixels>) -> f32 {
    (f32::from(bounds.size.width) - 2.0 * INSET - HANDLE_WIDTH).max(1.0)
}

fn snap(value: f64, step: f64) -> f64 {
    let snapped = (value / step).round() * step;
    // Round off the float error a multiple of 0.1 picks up, so the value
    // sent is the one shown.
    let scale = 10f64.powi(decimals(step) as i32);
    (snapped * scale).round() / scale
}

/// Decimal places a step needs to show.
fn decimals(step: f64) -> usize {
    (-step.log10().floor()).clamp(0.0, 9.0) as usize
}

/// `value` to the step's decimal places, or one more for a value a fine
/// drag left between steps. The tolerance is loose enough that an f32
/// field's widened value (0.3f32 is 0.30000001…) still counts as on a step.
fn format_value(value: f64, step: f64) -> String {
    let on_step = ((value / step) - (value / step).round()).abs() < 1e-3;
    let places = decimals(step) + usize::from(!on_step);
    format!("{value:.places$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrubber(value: f64, min: f64, max: f64, integer: bool) -> Scrubber {
        Scrubber::new(value, (min, max), integer, None)
    }

    #[test]
    fn the_step_is_a_hundredth_of_the_range() {
        assert_eq!(scrubber(0.0, 0.0, 1.0, false).step(), 0.01);
        assert_eq!(scrubber(0.0, 0.0, 1000.0, false).step(), 10.0);
        assert_eq!(scrubber(0.0, -50.0, 50.0, false).step(), 1.0);
        assert_eq!(
            scrubber(0.0, 0.0, 10.0, true).step(),
            1.0,
            "integers step whole"
        );
        assert_eq!(
            scrubber(1.0, 1.0, 1.0, false).step(),
            1.0,
            "an empty range still steps"
        );
    }

    #[test]
    fn a_drag_moves_by_distance_not_to_the_pointer() {
        let s = scrubber(0.5, 0.0, 1.0, false);
        assert_eq!(s.scrubbed(0.5, 50.0, 100.0, false), 1.0);
        assert_eq!(s.scrubbed(0.5, -25.0, 100.0, false), 0.25);
        assert_eq!(s.scrubbed(0.5, 50.0, 100.0, true), 0.55);
        assert_eq!(s.scrubbed(0.9, 50.0, 100.0, false), 1.0, "clamped");
    }

    #[test]
    fn shift_doesnt_make_an_integer_fractional() {
        let s = scrubber(100.0, 25.0, 200.0, true);
        assert_eq!(s.scrubbed(100.0, 1.0, 100.0, true), 100.0);
        assert_eq!(s.scrubbed(100.0, 10.0, 100.0, true), 102.0);
        assert_eq!(s.nudged(1.0, true), 101.0);
    }

    #[test]
    fn a_nudge_steps_one_notch() {
        assert_eq!(scrubber(0.5, 0.0, 1.0, false).nudged(1.0, false), 0.51);
        assert_eq!(scrubber(0.5, 0.0, 1.0, false).nudged(-1.0, true), 0.499);
        assert_eq!(
            scrubber(1.0, 0.0, 1.0, false).nudged(1.0, false),
            1.0,
            "clamped"
        );
        assert_eq!(
            scrubber(f64::NAN, 0.0, 1.0, false).nudged(1.0, false),
            0.01,
            "from min"
        );
    }

    #[test]
    fn a_bad_range_doesnt_panic() {
        scrubber(0.5, 1.0, 0.0, false).nudged(1.0, false);
        scrubber(0.5, f64::NAN, 1.0, false).nudged(1.0, false);
        assert_eq!(scrubber(0.5, 1.0, 0.0, false).fraction(), 0.0);
        assert!(!scrubber(0.5, 0.0, f64::INFINITY, false).ranged());
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

    #[test]
    fn an_f32_value_formats_like_its_f64() {
        assert_eq!(format_value(f64::from(0.3f32), 0.01), "0.30");
        assert_eq!(format_value(f64::from(0.9f32), 0.01), "0.90");
    }
}
