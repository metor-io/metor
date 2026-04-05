use std::{fmt, ops::Range, str::FromStr, time::Duration};

use metor_proto::types::Timestamp;

/// A time offset relative to the earliest or latest timestamp, or a fixed point.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Offset {
    Earliest(Duration),
    Latest(Duration),
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
            Offset::Fixed(ts) => write!(f, "={}", ts.0),
        }
    }
}

/// Describes how to compute the visible time window from the data's full range.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
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

    /// Common presets for the palette.
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

fn clamp_range(full: Range<Timestamp>, requested: Range<Timestamp>) -> Range<Timestamp> {
    let start = requested.start.max(full.start);
    let end = requested.end.min(full.end);
    if start >= end {
        full
    } else {
        start..end
    }
}

peg::parser! {
    grammar offset_parser() for str {
        rule _ = quiet!{[' ' | '\n' | '\t']*}
        rule parse_span() -> jiff::Span
            = str:$([_]+) {? str.parse().or(Err("invalid duration")) }

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
    Ok(Duration::from_nanos(
        span.total(jiff::Unit::Nanosecond)? as u64,
    ))
}

/// Parse a string like "- 30s ↔ - 0s" or "LAST 30s" into a [`TimeRangeBehavior`].
/// Also accepts single offset strings like "- 30s" (interpreted as start, end = latest).
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
