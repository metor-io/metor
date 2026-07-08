use std::{fmt, ops::Range, str::FromStr, time::Duration};

use metor_proto::types::Timestamp;

/// One endpoint of a [`TimeRangeBehavior`].
///
/// Relative endpoints track a moving window against live data; the
/// absolute `Fixed` form pins to a specific wall-clock timestamp.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Offset {
    /// `earliest_timestamp + duration`.
    Earliest(Duration),
    /// `latest_timestamp - duration`.
    Latest(Duration),
    /// Absolute epoch.
    Fixed(Timestamp),
}

impl fmt::Display for Offset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Offset::Earliest(duration) => {
                let d = hifitime::Duration::from(*duration)
                    .to_string()
                    .to_uppercase();
                write!(f, "+{d}")
            }
            Offset::Latest(duration) => {
                let d = hifitime::Duration::from(*duration)
                    .to_string()
                    .to_uppercase();
                write!(f, "-{d}")
            }
            Offset::Fixed(ts) => write!(f, "={}", hifitime::Epoch::from(*ts)),
        }
    }
}

/// Derivation rule for the visible time window.
///
/// Resolved against the data's earliest/latest timestamps via
/// [`TimeRangeBehavior::calculate_range`].
#[derive(Debug, PartialEq, Eq, Clone, Copy, facet::Facet)]
#[facet(opaque)]
pub struct TimeRangeBehavior {
    pub start: Offset,
    pub end: Offset,
}

impl Default for TimeRangeBehavior {
    fn default() -> Self {
        Self::FULL
    }
}

impl fmt::Display for TimeRangeBehavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.start, self.end) {
            (Offset::Earliest(start), Offset::Latest(end)) if end.is_zero() && start.is_zero() => {
                write!(f, "FULL RANGE")
            }
            (Offset::Latest(start), Offset::Latest(end)) if end.is_zero() => {
                let start = hifitime::Duration::from(start).to_string().to_uppercase();
                write!(f, "LAST {start}")
            }
            (start, end) => {
                write!(f, "{start} ↔ {end}")
            }
        }
    }
}

impl TimeRangeBehavior {
    pub const FULL: Self = TimeRangeBehavior {
        start: Offset::Earliest(Duration::ZERO),
        end: Offset::Latest(Duration::ZERO),
    };

    pub const fn last(duration: Duration) -> Self {
        TimeRangeBehavior {
            start: Offset::Latest(duration),
            end: Offset::Latest(Duration::ZERO),
        }
    }

    pub fn calculate_range(&self, earliest: Timestamp, latest: Timestamp) -> Range<Timestamp> {
        let start = match self.start {
            Offset::Earliest(duration) => earliest + duration,
            Offset::Latest(duration) => latest - duration,
            Offset::Fixed(timestamp) => timestamp,
        };
        let end = match self.end {
            Offset::Earliest(duration) => earliest + duration,
            Offset::Latest(duration) => latest - duration,
            Offset::Fixed(timestamp) => timestamp,
        };

        clamp_range(earliest..latest, start..end)
    }

    /// Preset choices offered by the inspector's range picker.
    pub const PRESETS: &[(&str, TimeRangeBehavior)] = &[
        ("Full Range", Self::FULL),
        ("Last 30s", Self::last(Duration::from_secs(30))),
        ("Last 1m", Self::last(Duration::from_secs(60))),
        ("Last 5m", Self::last(Duration::from_secs(300))),
        ("Last 15m", Self::last(Duration::from_secs(900))),
        ("Last 30m", Self::last(Duration::from_secs(1800))),
        ("Last 1h", Self::last(Duration::from_secs(3600))),
        ("Last 6h", Self::last(Duration::from_secs(3600 * 6))),
        ("Last 12h", Self::last(Duration::from_secs(3600 * 12))),
        ("Last 24h", Self::last(Duration::from_secs(3600 * 24))),
    ];
}

/// Inspector rows shared by every time-range chooser: one command per
/// [`TimeRangeBehavior::PRESETS`] entry plus a freeform text field
/// speaking the `FromStr` grammar. Callers prepend extra rows (e.g. an
/// "Auto" reset) and supply the setter.
pub fn picker_rows(
    current: gpui::SharedString,
    set: std::sync::Arc<dyn Fn(TimeRangeBehavior, &mut gpui::Window, &mut gpui::App)>,
) -> Vec<Box<dyn crate::inspector::rows::InspectorRow>> {
    use std::sync::Arc;

    use crate::inspector::rows::{CommandRow, InspectorRow, TextRow};

    let mut rows: Vec<Box<dyn InspectorRow>> = TimeRangeBehavior::PRESETS
        .iter()
        .map(|(name, value)| {
            let set = set.clone();
            let value = *value;
            Box::new(CommandRow::new(
                *name,
                Arc::new(move |window, cx| set(value, window, cx)),
            )) as Box<dyn InspectorRow>
        })
        .collect();
    rows.push(Box::new(TextRow::new(
        gpui::SharedString::new_static("Custom"),
        current,
        Arc::new(move |s, window, cx| {
            if let Ok(value) = s.parse::<TimeRangeBehavior>() {
                set(value, window, cx);
            }
        }),
    )));
    rows
}

fn clamp_range(full: Range<Timestamp>, requested: Range<Timestamp>) -> Range<Timestamp> {
    let start = requested.start.max(full.start);
    let end = requested.end.min(full.end);
    if start >= end { full } else { start..end }
}

/// App-wide default time window. Plots whose `x_range` is `Auto` follow
/// this; a per-plot `Custom` range wins. One narrowing gesture in the
/// toolbar moves every plot to the working window instead of demanding
/// per-plot edits.
pub struct GlobalTimeRange {
    pub behavior: TimeRangeBehavior,
}

impl gpui::Global for GlobalTimeRange {}

impl GlobalTimeRange {
    /// The current global behavior; full range until anyone narrows it.
    pub fn get(cx: &gpui::App) -> TimeRangeBehavior {
        cx.try_global::<Self>()
            .map(|g| g.behavior)
            .unwrap_or(TimeRangeBehavior::FULL)
    }

    pub fn set(cx: &mut gpui::App, behavior: TimeRangeBehavior) {
        cx.set_global(Self { behavior });
        cx.refresh_windows();
    }
}

peg::parser! {
    grammar offset_parser() for str {
        rule _ = quiet!{[' ' | '\n' | '\t']*}
        // `Display` uppercases durations ("30 S"); jiff only accepts
        // lowercase unit designators.
        rule parse_span() -> jiff::Span
            = str:$([_]+) {? str.to_lowercase().parse().or(Err("invalid duration")) }

        rule zero() -> jiff::Span
            = "0" ['s'|'m'|'h']? { jiff::Span::new() }

        rule span() -> jiff::Span
            = zero() / parse_span()

        rule epoch() -> hifitime::Epoch
            = str:$([_]+) {? str.parse().or(Err("invalid epoch")) }

        rule start() -> Offset
            = "+" _  span:span()  {? span_to_duration(span).map(Offset::Earliest).or(Err("invalid duration")) }
        rule end() -> Offset
            = "-" _ span:span()  {? span_to_duration(span).map(Offset::Latest).or(Err("invalid duration")) }

        rule fixed() -> Offset
            = "=" _ epoch:epoch()  { Offset::Fixed(Timestamp::from(epoch)) }

        pub rule offset() -> Offset
            = start() / end() / fixed()
    }
}

impl FromStr for Offset {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        offset_parser::offset(s).map_err(|_| ())
    }
}

fn span_to_duration(span: jiff::Span) -> Result<Duration, jiff::Error> {
    // Days are calendar units to jiff and normally need a reference date;
    // ranges here are wall-clock windows, so a day is always 24 hours
    // (and `Display` prints 24h as "1 DAY", which must parse back).
    let total = span.total(jiff::SpanTotal::from(jiff::Unit::Nanosecond).days_are_24_hours())?;
    Ok(Duration::from_nanos(total as u64))
}

/// Parse strings produced by `Display` plus a few ergonomic shortcuts.
///
/// Accepts `"full"`, `"last 30s"`, `"- 30s ↔ - 0s"`, or a single offset
/// (interpreted as the start, end defaults to the latest sample).
impl FromStr for TimeRangeBehavior {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("full") || s.eq_ignore_ascii_case("full range") {
            return Ok(TimeRangeBehavior::FULL);
        }

        // "LAST Xunit" shorthand
        let lower = s.to_lowercase();
        if let Some(rest) = lower.strip_prefix("last ") {
            let offset: Offset = format!("- {}", rest.trim()).parse().map_err(|_| ())?;
            if let Offset::Latest(dur) = offset {
                return Ok(TimeRangeBehavior::last(dur));
            }
        }

        // "offset ↔ offset" form
        if let Some((left, right)) = s.split_once('↔') {
            let start: Offset = left.trim().parse().map_err(|_| ())?;
            let end: Offset = right.trim().parse().map_err(|_| ())?;
            return Ok(TimeRangeBehavior { start, end });
        }

        // Single offset: use as start, end = latest
        let start: Offset = s.parse().map_err(|_| ())?;
        Ok(TimeRangeBehavior {
            start,
            end: Offset::Latest(Duration::ZERO),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_offset() {
        let o: Offset = "+ 0s".parse().unwrap();
        assert_eq!(o, Offset::Earliest(Duration::from_secs(0)));
        let o: Offset = "+ 20s".parse().unwrap();
        assert_eq!(o, Offset::Earliest(Duration::from_secs(20)));
        let o: Offset = "- 20s".parse().unwrap();
        assert_eq!(o, Offset::Latest(Duration::from_secs(20)));
    }

    /// Layout persistence stores ranges as `Display` text and reloads via
    /// `FromStr`; every preset must survive the round trip.
    #[test]
    fn presets_round_trip_through_display() {
        for (name, preset) in TimeRangeBehavior::PRESETS {
            let text = preset.to_string();
            let parsed: TimeRangeBehavior = text
                .parse()
                .unwrap_or_else(|_| panic!("{name}: {text:?} failed to parse"));
            assert_eq!(parsed, *preset, "{name} changed across the round trip");
        }
    }

    /// Pinned and mixed ranges persist the same way as presets; absolute
    /// endpoints must survive Display → FromStr without drifting.
    #[test]
    fn fixed_and_mixed_round_trip_through_display() {
        // Sub-millisecond endpoints catch lossy f64 Epoch conversions that
        // millisecond-aligned values can't.
        let pinned = TimeRangeBehavior {
            start: Offset::Fixed(Timestamp(1_780_000_000_000_001)),
            end: Offset::Fixed(Timestamp(1_780_000_060_123_457)),
        };
        let mixed = TimeRangeBehavior {
            start: Offset::Earliest(Duration::from_secs(5)),
            end: Offset::Latest(Duration::from_secs(10)),
        };
        for behavior in [pinned, mixed] {
            let text = behavior.to_string();
            let parsed: TimeRangeBehavior = text
                .parse()
                .unwrap_or_else(|_| panic!("{text:?} failed to parse"));
            assert_eq!(parsed, behavior, "{text:?} changed across the round trip");
        }
    }

    #[test]
    fn parse_time_range_behavior() {
        let b: TimeRangeBehavior = "full range".parse().unwrap();
        assert_eq!(b, TimeRangeBehavior::FULL);

        let b: TimeRangeBehavior = "last 30s".parse().unwrap();
        assert_eq!(b, TimeRangeBehavior::last(Duration::from_secs(30)));

        let b: TimeRangeBehavior = "LAST 5m".parse().unwrap();
        assert_eq!(b, TimeRangeBehavior::last(Duration::from_secs(300)));
    }
}
