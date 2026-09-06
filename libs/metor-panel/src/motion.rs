//! Short opacity transitions whose lifetime follows their owning view.
use std::time::{Duration, Instant};

use gpui::{App, Window};

use crate::config::MotionPreference;

pub(crate) const PALETTE_ENTER: Duration = Duration::from_millis(120);
pub(crate) const PALETTE_EXIT: Duration = Duration::from_millis(80);
pub(crate) const MENU_ENTER: Duration = Duration::from_millis(70);
pub(crate) const MENU_EXIT: Duration = Duration::from_millis(90);
pub(crate) const TRANSIENT_ENTER: Duration = Duration::from_millis(100);

/// Emitted when an overlay's visual can be released, after logical dismissal.
pub(crate) struct Closed;

pub(crate) fn enabled(cx: &App) -> bool {
    cx.try_global::<crate::theme::FontSettings>()
        .is_none_or(|settings| settings.config.motion != MotionPreference::Reduced)
}

pub(crate) fn set_preference(preference: MotionPreference, cx: &mut App) {
    let settings = cx.global_mut::<crate::theme::FontSettings>();
    settings.config.motion = preference;
    if let Err(error) = crate::config::save(&settings.config) {
        tracing::error!(%error, "save motion preference failed");
    }
    cx.refresh_windows();
}

/// Retains opacity across renders and samples a new target from the current value.
pub(crate) struct Fade {
    from: f32,
    target: f32,
    started: Option<Instant>,
    duration: Duration,
}

impl Fade {
    pub(crate) fn settled(opacity: f32) -> Self {
        Self {
            from: opacity,
            target: opacity,
            started: None,
            duration: Duration::ZERO,
        }
    }

    pub(crate) fn entrance(duration: Duration) -> Self {
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

    pub(crate) fn finish(&mut self) {
        self.from = self.target;
        self.duration = Duration::ZERO;
    }

    pub(crate) fn exit(&mut self, duration: Duration) {
        self.retarget(0.0, duration, Instant::now());
    }

    pub(crate) fn enter(&mut self, duration: Duration) {
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

    pub(crate) fn opacity(&mut self, window: &Window, cx: &App) -> f32 {
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
