use serde::{Deserialize, Serialize};

/// Integer time bounds keep epoch precision out of screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval {
    pub start: i64,
    pub end: i64,
}

impl Interval {
    pub fn new(start: i64, end: i64) -> Self {
        let start = start.min(i64::MAX - 1);
        Self {
            start,
            end: end.max(start + 1),
        }
    }

    pub fn span(self) -> f64 {
        (i128::from(self.end) - i128::from(self.start)) as f64
    }
    pub fn fraction(self, t: i64) -> f64 {
        (i128::from(t) - i128::from(self.start)) as f64 / self.span()
    }
    pub fn at(self, fraction: f64) -> i64 {
        clamp(i128::from(self.start) + (self.span() * fraction).round() as i128)
    }
    pub fn shifted(self, delta: i128) -> Self {
        let delta = delta.clamp(
            i128::from(i64::MIN) - i128::from(self.start),
            i128::from(i64::MAX) - i128::from(self.end),
        );
        Self::new(
            clamp(i128::from(self.start) + delta),
            clamp(i128::from(self.end) + delta),
        )
    }
    pub fn zoom(self, factor: f64, fraction: f64) -> Self {
        let anchor = self.at(fraction);
        let span = (self.span() * factor).clamp(
            1_000.0,
            (i128::from(i64::MAX) - i128::from(i64::MIN)) as f64,
        );
        let start = clamp(i128::from(anchor) - (span * fraction).round() as i128);
        Self::new(start, clamp(i128::from(start) + span.round() as i128))
    }
    pub fn hull(self, other: Self) -> Self {
        Self::new(self.start.min(other.start), self.end.max(other.end))
    }
    pub fn padded(self) -> Self {
        let span = self.span().max(1_000_000.0);
        Self::new(
            clamp(i128::from(self.start) - (span * 0.05) as i128),
            clamp(i128::from(self.start) + (span * 1.15) as i128),
        )
    }
}

pub(super) fn clamp(value: i128) -> i64 {
    value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// Navigation belongs to each widget, independently of the global selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Navigation {
    #[default]
    Fit,
    Manual(Interval),
    Follow {
        span: f64,
    },
}

pub(super) fn tick_step(span: f64, width: f32) -> i64 {
    let desired = span / (f64::from(width) / 115.0).max(1.0);
    const STEPS: &[i64] = &[
        1_000,
        2_000,
        5_000,
        10_000,
        20_000,
        50_000,
        100_000,
        200_000,
        500_000,
        1_000_000,
        2_000_000,
        5_000_000,
        10_000_000,
        15_000_000,
        30_000_000,
        60_000_000,
        120_000_000,
        300_000_000,
        600_000_000,
        900_000_000,
        1_800_000_000,
        3_600_000_000,
        7_200_000_000,
        21_600_000_000,
        43_200_000_000,
        86_400_000_000,
    ];
    STEPS
        .iter()
        .copied()
        .find(|s| *s as f64 >= desired)
        .unwrap_or_else(|| {
            (86_400_000_000.0 * (desired / 86_400_000_000.0).ceil()).min(i64::MAX as f64) as i64
        })
}

pub(super) fn edge_speed(fraction: f64, width: f32) -> f64 {
    let edge = (24.0 / f64::from(width.max(1.0))).min(0.25);
    if fraction < edge {
        -((edge - fraction) / edge).clamp(0.0, 1.0).powi(2) * 0.5
    } else if fraction > 1.0 - edge {
        ((fraction - (1.0 - edge)) / edge).clamp(0.0, 1.0).powi(2) * 0.5
    } else {
        0.0
    }
}

pub(super) fn ticks(view: Interval, step: i64, timezone: &str, origin: Option<i64>) -> Vec<i64> {
    let mut ticks = Vec::new();
    if step >= 86_400_000_000 && origin.is_none() {
        let zone = if timezone.eq_ignore_ascii_case("local") {
            jiff::tz::TimeZone::system()
        } else {
            jiff::tz::TimeZone::get(timezone).unwrap_or(jiff::tz::TimeZone::UTC)
        };
        if let Ok(timestamp) = jiff::Timestamp::from_microsecond(view.start) {
            let mut date = timestamp.to_zoned(zone.clone()).date();
            for _ in 0..100 {
                let Ok(start) = date.to_zoned(zone.clone()) else {
                    break;
                };
                let t = start.timestamp().as_microsecond();
                if t > view.end {
                    break;
                }
                if t >= view.start {
                    ticks.push(t);
                }
                let Ok(next) = date.checked_add(jiff::Span::new().days(step / 86_400_000_000))
                else {
                    break;
                };
                date = next;
            }
            return ticks;
        }
    }
    let origin = i128::from(origin.unwrap_or(0));
    let step = i128::from(step.max(1));
    let mut tick = (i128::from(view.start) - origin).div_euclid(step) * step + origin;
    for _ in 0..100 {
        if tick > i128::from(view.end) {
            break;
        }
        if tick >= i128::from(view.start) {
            ticks.push(clamp(tick));
        }
        tick += step;
    }
    ticks
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn civil_day_ticks_cross_dst_and_elapsed_ticks_start_at_t0() {
        let a = "2026-03-07T00:00:00-08:00[America/Los_Angeles]"
            .parse::<jiff::Zoned>()
            .unwrap()
            .timestamp()
            .as_microsecond();
        let b = "2026-03-10T00:00:00-07:00[America/Los_Angeles]"
            .parse::<jiff::Zoned>()
            .unwrap()
            .timestamp()
            .as_microsecond();
        let points = ticks(
            Interval::new(a, b),
            86_400_000_000,
            "America/Los_Angeles",
            None,
        );
        assert_eq!(points.len(), 4);
        assert_eq!(points[2] - points[1], 23 * 3_600_000_000);
        assert_eq!(
            ticks(
                Interval::new(0, 10_000_000),
                2_000_000,
                "UTC",
                Some(1_000_123)
            )[0],
            1_000_123
        );
    }
    #[test]
    fn epoch_precision_and_extreme_shifts() {
        let v = Interval::new(1_789_000_000_000_000, 1_789_000_000_001_000);
        for offset in [0, 1, 499, 999, 1000] {
            assert_eq!(v.at(v.fraction(v.start + offset)), v.start + offset);
        }
        let v = Interval::new(i64::MAX - 10, i64::MAX).shifted(100);
        assert_eq!(v.end - v.start, 10);
        assert!(Interval::new(i64::MAX, i64::MIN).start < i64::MAX);
    }
    #[test]
    fn zoom_preserves_pointer_and_edge_motion_is_bounded() {
        let v = Interval::new(1_000_000, 11_000_000);
        assert_eq!(v.at(0.2), v.zoom(0.5, 0.2).at(0.2));
        assert_eq!(edge_speed(0.5, 500.0), 0.0);
        assert_eq!(edge_speed(2.0, 500.0), 0.5);
        for hz in [30, 60, 120] {
            let distance: f64 = (0..hz).map(|_| edge_speed(1.0, 500.0) / hz as f64).sum();
            assert!((distance - 0.5).abs() < 0.000001);
        }
    }
}
