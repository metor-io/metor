//! Short opacity and height transitions whose lifetime follows their owning view.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{AnyElement, App, IntoElement, Pixels, Window, canvas, div, prelude::*};
use web_time::Instant;

use crate::UiSettings;

/// Whether decorative fades run or UI state is shown immediately.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MotionPreference {
    #[default]
    Full,
    Reduced,
}

pub const PALETTE_ENTER: Duration = Duration::from_millis(120);
pub const PALETTE_EXIT: Duration = Duration::from_millis(80);
pub const MENU_ENTER: Duration = Duration::from_millis(70);
pub const MENU_EXIT: Duration = Duration::from_millis(90);
pub const TRANSIENT_ENTER: Duration = Duration::from_millis(100);
/// A page sliding in as it is entered or returned to.
pub const PAGE_TURN: Duration = Duration::from_millis(140);

/// Emitted when an overlay's visual can be released, after logical dismissal.
pub struct Closed;

/// Whether fades run under the current [`UiSettings`].
pub fn enabled(cx: &App) -> bool {
    cx.try_global::<UiSettings>()
        .is_none_or(|settings| settings.motion != MotionPreference::Reduced)
}

/// Change the motion preference and redraw every window under it.
pub fn set_preference(preference: MotionPreference, cx: &mut App) {
    cx.default_global::<UiSettings>().motion = preference;
    cx.refresh_windows();
}

/// Retains opacity across renders and samples a new target from the current value.
pub struct Fade {
    from: f32,
    target: f32,
    started: Option<Instant>,
    duration: Duration,
}

impl Fade {
    pub fn settled(opacity: f32) -> Self {
        Self {
            from: opacity,
            target: opacity,
            started: None,
            duration: Duration::ZERO,
        }
    }

    pub fn entrance(duration: Duration) -> Self {
        Self {
            from: 0.0,
            target: 1.0,
            started: None,
            duration,
        }
    }

    fn sample(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return self.target;
        }
        let Some(started) = self.started else {
            return self.from;
        };
        let progress = (now.saturating_duration_since(started).as_secs_f32()
            / self.duration.as_secs_f32())
        .min(1.0);
        if progress >= 1.0 {
            return self.target;
        }
        let eased = if self.target > self.from {
            1.0 - (1.0 - progress).powi(3)
        } else {
            progress
        };
        self.from + (self.target - self.from) * eased
    }

    pub fn finish(&mut self) {
        self.from = self.target;
        self.duration = Duration::ZERO;
    }

    pub fn exit(&mut self, duration: Duration) {
        self.retarget(0.0, duration, Instant::now());
    }

    pub fn enter(&mut self, duration: Duration) {
        self.retarget(1.0, duration, Instant::now());
    }

    fn retarget(&mut self, target: f32, duration: Duration, now: Instant) {
        self.from = self.sample(now);
        self.target = target;
        self.started = Some(now);
        self.duration = if self.from == target {
            Duration::ZERO
        } else {
            duration
        };
    }

    pub fn opacity(&mut self, window: &Window, cx: &App) -> f32 {
        let now = Instant::now();
        self.started.get_or_insert(now);
        if !enabled(cx) {
            self.finish();
        }
        let opacity = self.sample(now);
        if opacity != self.target {
            window.request_animation_frame();
        }
        opacity
    }
}

/// A collapsible section sliding open or shut.
pub const SECTION_SLIDE: Duration = Duration::from_millis(140);

/// A collapsible section's openness, eased so the section slides down as it opens and up as
/// it closes.
///
/// The section keeps its content's height from the last frame it was laid out, so the owner
/// renders the content while it is [`visible`](Self::visible), including while it closes.
pub struct Reveal {
    fade: Fade,
    open: bool,
    /// The content's height at its last layout.
    height: Rc<Cell<Pixels>>,
}

impl Reveal {
    /// A section that starts open, without sliding.
    pub fn open() -> Self {
        Self {
            fade: Fade::settled(1.0),
            open: true,
            height: Rc::default(),
        }
    }

    /// A section that starts shut.
    pub fn closed() -> Self {
        Self {
            fade: Fade::settled(0.0),
            open: false,
            height: Rc::default(),
        }
    }

    /// A section that slides open as it first appears.
    pub fn opening() -> Self {
        Self {
            fade: Fade::entrance(SECTION_SLIDE),
            open: true,
            height: Rc::default(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Slides the section open or shut, from wherever it is now.
    pub fn set_open(&mut self, open: bool) {
        if open == self.open {
            return;
        }
        self.open = open;
        if open {
            self.fade.enter(SECTION_SLIDE);
        } else {
            self.fade.exit(SECTION_SLIDE);
        }
    }

    /// Whether any of the section shows: it is open or still closing.
    pub fn visible(&self) -> bool {
        self.open || self.fade.sample(Instant::now()) > 0.0
    }

    /// `content` clipped to the part of its height the section has slid open, or `None` once
    /// it is shut.
    pub fn render(
        &mut self,
        content: impl IntoElement,
        window: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let amount = self.fade.opacity(window, cx);
        if amount == 0.0 && !self.open {
            return None;
        }
        let height = self.height.clone();
        let measure = canvas(
            move |bounds, _, _| height.set(bounds.size.height),
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        if amount >= 1.0 {
            return Some(
                div()
                    .flex_none()
                    .relative()
                    .child(content)
                    .child(measure)
                    .into_any_element(),
            );
        }
        // While sliding, the content lays out at its natural height out of flow and the
        // section shows the top of it.
        Some(
            div()
                .flex_none()
                .relative()
                .overflow_hidden()
                .h(self.height.get() * amount)
                .opacity(amount)
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .w_full()
                        .child(content)
                        .child(measure),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_entry_exits_from_its_current_opacity() {
        let start = Instant::now();
        let mut fade = Fade::entrance(PALETTE_ENTER);
        fade.started = Some(start);
        let halfway = start + PALETTE_ENTER / 2;
        let opacity = fade.sample(halfway);
        assert!(opacity > 0.0 && opacity < 1.0);
        fade.retarget(0.0, MENU_EXIT, halfway);
        assert_eq!(fade.sample(halfway), opacity);
        assert!((fade.sample(halfway + MENU_EXIT / 2) - opacity / 2.0).abs() < 1e-6);
        assert_eq!(fade.sample(halfway + MENU_EXIT), 0.0);
        assert_eq!(fade.sample(halfway + Duration::from_secs(2)), 0.0);
    }

    #[test]
    fn sampling_does_not_depend_on_frame_count_and_zero_duration_settles() {
        let start = Instant::now();
        let mut fade = Fade::entrance(PALETTE_ENTER);
        fade.started = Some(start);
        let expected = fade.sample(start + Duration::from_millis(90));
        for millis in 0..90 {
            fade.sample(start + Duration::from_millis(millis));
        }
        assert_eq!(fade.sample(start + Duration::from_millis(90)), expected);
        fade.finish();
        assert_eq!(fade.sample(start), 1.0);
        fade.retarget(0.0, Duration::ZERO, start);
        assert_eq!(fade.sample(start), 0.0);
    }
}
