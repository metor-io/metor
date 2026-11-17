//! A zoomed pane growing over its tab's layout and shrinking back.
use std::time::{Duration, Instant};

/// How long a pane takes to grow over the layout, or shrink back into it.
const ZOOM: Duration = Duration::from_millis(180);

/// A pane growing over the layout as it zooms, or shrinking back into its
/// slot as the zoom ends. The pane is laid out at each step's size, since a
/// gpui element can't be scaled as a picture.
pub(super) struct ZoomMotion {
    pub pane: gpui::Entity<super::Pane>,
    /// How much of the group the pane covered when this motion began: `0`
    /// is its slot in the layout, `1` is the whole group.
    from: f32,
    to: f32,
    started: Instant,
}

impl ZoomMotion {
    /// Move `pane` toward covering the group (`zooming`) or toward its slot,
    /// starting from wherever `previous` had it, so reversing mid-way turns
    /// around rather than jumping.
    pub(super) fn new(
        pane: gpui::Entity<super::Pane>,
        zooming: bool,
        previous: Option<&ZoomMotion>,
        now: Instant,
    ) -> Self {
        let from = previous
            .filter(|motion| motion.pane == pane)
            .and_then(|motion| motion.coverage(now))
            .unwrap_or(if zooming { 0.0 } else { 1.0 });
        Self {
            pane,
            from,
            to: if zooming { 1.0 } else { 0.0 },
            started: now,
        }
    }

    pub(super) fn zooming(&self) -> bool {
        self.to == 1.0
    }

    /// How much of the group the pane covers at `now`, eased out; `None`
    /// once it has arrived.
    pub(super) fn coverage(&self, now: Instant) -> Option<f32> {
        let progress = now.saturating_duration_since(self.started).as_secs_f32()
            / (ZOOM.as_secs_f32() * (self.to - self.from).abs().max(f32::EPSILON));
        if progress >= 1.0 {
            return None;
        }
        let eased = 1.0 - (1.0 - progress).powi(3);
        Some(self.from + (self.to - self.from) * eased)
    }
}

