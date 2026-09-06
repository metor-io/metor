//! Time expressions retain their anchors across seeks, clock ticks and saves.
use std::{fmt, ops::Range};

use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};

/// The reference instant used by an endpoint expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Anchor {
    DataStart,
    DataEnd,
    Live,
    View,
    Timestamp(i64),
}

/// An anchored instant with an exact, signed microsecond offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeExpr {
    pub anchor: Anchor,
    pub offset: i64,
}

impl TimeExpr {
    pub const LIVE: Self = Self::new(Anchor::Live, 0);
    pub const fn new(anchor: Anchor, offset: i64) -> Self {
        Self { anchor, offset }
    }
    pub const fn fixed(ts: Timestamp) -> Self {
        Self::new(Anchor::Timestamp(ts.0), 0)
    }
    pub fn resolve(self, context: &TimeContext) -> Result<Timestamp, String> {
        let base = match self.anchor {
            Anchor::DataStart => context.extent.as_ref().map(|e| e.start.0),
            Anchor::DataEnd => context.extent.as_ref().map(|e| e.end.0),
            Anchor::Live => context.live.map(|t| t.0),
            Anchor::View => context.view.map(|t| t.0),
            Anchor::Timestamp(t) => Some(t),
        }
        .ok_or_else(|| "Waiting for the selected clock or data bounds".to_string())?;
        base.checked_add(self.offset)
            .map(Timestamp)
            .ok_or_else(|| "Timestamp overflow".into())
    }
}

/// Independent visible endpoints; DataEnd denotes the last sample, not now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRangeSpec {
    pub start: TimeExpr,
    pub end: TimeExpr,
}

impl Default for TimeRangeSpec {
    fn default() -> Self {
        Self::FULL
    }
}
impl TimeRangeSpec {
    pub const FULL: Self = Self {
        start: TimeExpr::new(Anchor::DataStart, 0),
        end: TimeExpr::new(Anchor::DataEnd, 0),
    };
    pub fn resolve(self, context: &TimeContext) -> Result<Range<Timestamp>, String> {
        let start = self.start.resolve(context)?;
        let mut end = self.end.resolve(context)?;
        if start > end || (start == end && self != Self::FULL) {
            return Err("Start must be before end".into());
        }
        if self.end.anchor == Anchor::DataEnd && self.end.offset == 0 {
            end = Timestamp(end.0.checked_add(1).ok_or("Timestamp overflow")?);
        }
        Ok(start..end)
    }
    pub fn fixed(range: Range<Timestamp>) -> Self {
        Self {
            start: TimeExpr::fixed(range.start),
            end: TimeExpr::fixed(range.end),
        }
    }
}

/// One clock snapshot for all endpoint resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimeContext {
    pub extent: Option<Range<Timestamp>>,
    pub live: Option<Timestamp>,
    pub view: Option<Timestamp>,
}

/// Editing context freezes shorthand dates while Live previews continue moving.
#[derive(Clone)]
pub struct ParseContext {
    pub zone: jiff::tz::TimeZone,
    pub today: jiff::civil::Date,
    pub view_date: jiff::civil::Date,
}

impl ParseContext {
    pub fn new(zone: &str, now: Timestamp, view: Timestamp) -> Result<Self, String> {
        let zone = if zone.eq_ignore_ascii_case("local") {
            jiff::tz::TimeZone::system()
        } else {
            jiff::tz::TimeZone::get(zone).map_err(|e| e.to_string())?
        };
        let date = |ts: Timestamp| {
            jiff::Timestamp::from_microsecond(ts.0)
                .map(|t| t.to_zoned(zone.clone()).date())
                .map_err(|e| e.to_string())
        };
        Ok(Self {
            today: date(now)?,
            view_date: date(view)?,
            zone,
        })
    }
    pub fn utc() -> Self {
        Self::new("UTC", Timestamp::now(), Timestamp::now()).expect("current UTC time")
    }
}

/// Parse exact elapsed durations without floating-point rounding or calendar units.
pub fn duration(text: &str) -> Result<i64, String> {
    let lower = text.trim().to_ascii_lowercase();
    let mut rest = lower.as_str();
    let mut total: i128 = 0;
    if rest.is_empty() {
        return Err("Enter a duration, such as 2.5m".into());
    }
    while !rest.is_empty() {
        let n = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let number = &rest[..n];
        if number.is_empty() {
            return Err("Expected a positive number and unit".into());
        }
        rest = rest[n..].trim_start();
        let n = rest
            .find(|c: char| !c.is_ascii_alphabetic() && c != 'µ')
            .unwrap_or(rest.len());
        let multiplier: i128 = match &rest[..n] {
            "us" | "µs" => 1,
            "ms" => 1_000,
            "s" | "sec" | "second" | "seconds" => 1_000_000,
            "m" | "min" | "minute" | "minutes" => 60_000_000,
            "h" | "hr" | "hour" | "hours" => 3_600_000_000,
            "d" | "day" | "days" => 86_400_000_000,
            _ => return Err("Use us, ms, s, m, h or d (m means minutes)".into()),
        };
        let (whole, frac) = number.split_once('.').unwrap_or((number, ""));
        let whole: i128 = whole.parse().map_err(|_| "Invalid duration")?;
        if frac.len() > 18 || (!frac.is_empty() && !frac.bytes().all(|b| b.is_ascii_digit())) {
            return Err("Invalid duration precision".into());
        }
        let scale = 10_i128.pow(frac.len() as u32);
        let frac = if frac.is_empty() {
            0
        } else {
            frac.parse::<i128>().map_err(|_| "Invalid duration")?
        };
        let ticks = whole.checked_mul(multiplier).ok_or("Duration overflow")?;
        let fractional = frac.checked_mul(multiplier).ok_or("Duration overflow")?;
        if fractional % scale != 0 {
            return Err("Duration must resolve to whole microseconds".into());
        }
        total = total
            .checked_add(ticks)
            .and_then(|n| n.checked_add(fractional / scale))
            .ok_or("Duration overflow")?;
        rest = rest[n..].trim_start();
    }
    i64::try_from(total).map_err(|_| "Duration overflow".into())
}

pub fn format_duration(us: i64) -> String {
    let us = us.unsigned_abs();
    for (unit, divisor) in [
        ("d", 86_400_000_000),
        ("h", 3_600_000_000),
        ("m", 60_000_000),
        ("s", 1_000_000),
        ("ms", 1_000),
    ] {
        if us >= divisor && us % divisor == 0 {
            return format!("{}{unit}", us / divisor);
        }
    }
    format!("{us}us")
}

pub fn timestamp_text(ts: Timestamp, zone: &str) -> String {
    let zone = if zone.eq_ignore_ascii_case("local") {
        jiff::tz::TimeZone::system()
    } else {
        jiff::tz::TimeZone::get(zone).unwrap_or(jiff::tz::TimeZone::UTC)
    };
    jiff::Timestamp::from_microsecond(ts.0)
        .map(|t| {
            t.to_zoned(zone)
                .strftime("%Y-%m-%d %H:%M:%S%.6f %:z")
                .to_string()
        })
        .unwrap_or_else(|_| format!("{}us since epoch", ts.0))
}

impl fmt::Display for TimeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.anchor {
            Anchor::DataStart => f.write_str("data start")?,
            Anchor::DataEnd => f.write_str("data end")?,
            Anchor::Live => f.write_str("live")?,
            Anchor::View => f.write_str("view time")?,
            Anchor::Timestamp(t) => f.write_str(&timestamp_text(Timestamp(t), "UTC"))?,
        }
        if self.offset != 0 {
            write!(
                f,
                " {} {}",
                if self.offset < 0 { "-" } else { "+" },
                format_duration(self.offset)
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for TimeRangeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Self::FULL {
            return f.write_str("full range");
        }
        if self.start.anchor == Anchor::DataStart
            && self.start.offset == 0
            && self.end.anchor == Anchor::DataStart
            && self.end.offset > 0
        {
            return write!(f, "first {}", format_duration(self.end.offset));
        }
        if self.start.anchor == self.end.anchor && self.start.offset < 0 && self.end.offset == 0 {
            let d = format_duration(self.start.offset);
            match self.end.anchor {
                Anchor::Live => return write!(f, "last {d}"),
                Anchor::DataEnd => return write!(f, "last {d} of data"),
                Anchor::View => return write!(f, "{d} ending at view time"),
                _ => {}
            }
        }
        write!(f, "{} .. {}", self.start, self.end)
    }
}

pub fn parse_instant(
    text: &str,
    context: &ParseContext,
    allow_view: bool,
) -> Result<TimeExpr, String> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    for (name, anchor) in [
        ("data start", Anchor::DataStart),
        ("data end", Anchor::DataEnd),
        ("view time", Anchor::View),
        ("live", Anchor::Live),
    ] {
        if let Some(rest) = lower.strip_prefix(name) {
            if anchor == Anchor::View && !allow_view {
                return Err("View time cannot reference itself".into());
            }
            let rest = rest.trim();
            let offset = if rest.is_empty() {
                0
            } else if let Some(d) = rest.strip_prefix('+') {
                duration(d)?
            } else if let Some(d) = rest.strip_prefix('-') {
                duration(d)?.checked_neg().ok_or("Duration overflow")?
            } else {
                return Err("Expected + or - followed by a duration".into());
            };
            return Ok(TimeExpr::new(anchor, offset));
        }
    }
    if let Some((base, d)) = text.rsplit_once(" + ") {
        let mut expr = parse_instant(base, context, allow_view)?;
        expr.offset = expr
            .offset
            .checked_add(duration(d)?)
            .ok_or("Duration overflow")?;
        return Ok(expr);
    }
    if let Some((base, d)) = text.rsplit_once(" - ") {
        let mut expr = parse_instant(base, context, allow_view)?;
        expr.offset = expr
            .offset
            .checked_sub(duration(d)?)
            .ok_or("Duration overflow")?;
        return Ok(expr);
    }
    if let Some(d) = text.strip_prefix('+') {
        return Ok(TimeExpr::new(Anchor::DataStart, duration(d)?));
    }
    if let Some(d) = text.strip_prefix('-') {
        return Ok(TimeExpr::new(Anchor::DataEnd, -duration(d)?));
    }
    if let Some(epoch) = text.strip_prefix('=') {
        return epoch
            .parse::<hifitime::Epoch>()
            .map(|e| TimeExpr::fixed(Timestamp::from(e)))
            .map_err(|e| e.to_string());
    }
    let mut text = text.to_string();
    if lower.starts_with("today ") {
        text = format!("{} {}", context.today, &text[6..]);
    }
    if lower.starts_with("yesterday ") {
        text = format!(
            "{} {}",
            context
                .today
                .checked_sub(jiff::Span::new().days(1))
                .map_err(|e| e.to_string())?,
            &text[10..]
        );
    }
    if text.as_bytes().get(2) == Some(&b':') {
        text = format!("{} {text}", context.view_date);
    }
    if text.len() <= 10 {
        return Err("Enter a date and time, such as 2026-09-05 14:30:00".into());
    }
    // Explicit offsets are parsed before civil times, so repeated local times can be selected precisely.
    let normalized = text
        .replacen(' ', "T", 1)
        .replace(" UTC", "Z")
        .replace(" utc", "Z")
        .replace(" +", "+")
        .replace(" -", "-");
    if normalized.contains(":60") {
        return Err("Leap-second entry is not supported".into());
    }
    if let Ok(stamp) = normalized.parse::<jiff::Timestamp>() {
        if stamp.subsec_nanosecond() % 1000 != 0 {
            return Err("Use microsecond precision or coarser".into());
        }
        return Ok(TimeExpr::fixed(Timestamp(stamp.as_microsecond())));
    }
    let (civil, zone) = if let Some((civil, zone)) = text.rsplit_once(' ') {
        if zone.contains('/') || zone.eq_ignore_ascii_case("local") {
            (
                civil.to_string(),
                if zone.eq_ignore_ascii_case("local") {
                    jiff::tz::TimeZone::system()
                } else {
                    jiff::tz::TimeZone::get(zone).map_err(|e| e.to_string())?
                },
            )
        } else {
            (text, context.zone.clone())
        }
    } else {
        (text, context.zone.clone())
    };
    let civil: jiff::civil::DateTime = civil
        .replacen(' ', "T", 1)
        .parse()
        .map_err(|_| "Enter YYYY-MM-DD HH:MM:SS with an optional timezone")?;
    let stamp = zone
        .to_ambiguous_zoned(civil)
        .unambiguous()
        .map_err(|_| "Ambiguous or nonexistent local time; choose an explicit UTC offset")?
        .timestamp();
    if stamp.subsec_nanosecond() % 1000 != 0 {
        return Err("Use microsecond precision or coarser".into());
    }
    Ok(TimeExpr::fixed(Timestamp(stamp.as_microsecond())))
}

pub fn parse_range(text: &str, context: &ParseContext) -> Result<TimeRangeSpec, String> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let lower = lower.strip_prefix("the ").unwrap_or(&lower);
    if matches!(lower, "full" | "full range") {
        return Ok(TimeRangeSpec::FULL);
    }
    let window = |anchor, start, end| TimeRangeSpec {
        start: TimeExpr::new(anchor, start),
        end: TimeExpr::new(anchor, end),
    };
    if let Some(d) = lower.strip_prefix("first ") {
        let d = positive_duration(d)?;
        return Ok(window(Anchor::DataStart, 0, d));
    }
    if let Some(d) = lower.strip_prefix("last ") {
        let (d, anchor) = d
            .strip_suffix(" of data")
            .map(|d| (d, Anchor::DataEnd))
            .unwrap_or((d, Anchor::Live));
        return Ok(window(anchor, -positive_duration(d)?, 0));
    }
    if let Some(d) = lower.strip_suffix(" ending at view time") {
        return Ok(window(Anchor::View, -positive_duration(d)?, 0));
    }
    if let Some(d) = lower.strip_suffix(" around view time") {
        let d = positive_duration(d)?;
        return Ok(window(Anchor::View, -(d / 2), d - d / 2));
    }
    if let Some(day) = lower
        .strip_prefix("day ")
        .or_else(|| trimmed.parse::<jiff::civil::Date>().ok().map(|_| trimmed))
    {
        let day: jiff::civil::Date = day.parse().map_err(|_| "Use day YYYY-MM-DD")?;
        let next = day
            .checked_add(jiff::Span::new().days(1))
            .map_err(|e| e.to_string())?;
        let at = |date: jiff::civil::Date| {
            context
                .zone
                .to_ambiguous_zoned(date.at(0, 0, 0, 0))
                .unambiguous()
                .map(|z| TimeExpr::fixed(Timestamp(z.timestamp().as_microsecond())))
                .map_err(|e| e.to_string())
        };
        return Ok(TimeRangeSpec {
            start: at(day)?,
            end: at(next)?,
        });
    }
    let (start, end) = trimmed
        .split_once("..")
        .or_else(|| trimmed.split_once('↔'))
        .ok_or("Enter first/last duration, or START .. END")?;
    Ok(TimeRangeSpec {
        start: parse_instant(start, context, true)?,
        end: parse_instant(end, context, true)?,
    })
}

fn positive_duration(text: &str) -> Result<i64, String> {
    let d = duration(text)?;
    if d > 0 {
        Ok(d)
    } else {
        Err("Duration must be greater than zero".into())
    }
}

impl From<crate::views::time_series::TimeRangeBehavior> for TimeRangeSpec {
    fn from(old: crate::views::time_series::TimeRangeBehavior) -> Self {
        use crate::views::time_series::time_range::Offset;
        let endpoint = |o| match o {
            Offset::Earliest(d) => TimeExpr::new(
                Anchor::DataStart,
                d.as_micros().min(i64::MAX as u128) as i64,
            ),
            Offset::Latest(d) => TimeExpr::new(
                Anchor::DataEnd,
                -(d.as_micros().min(i64::MAX as u128) as i64),
            ),
            Offset::Fixed(t) => TimeExpr::fixed(t),
            Offset::Expression(expr) => expr,
        };
        Self {
            start: endpoint(old.start),
            end: endpoint(old.end),
        }
    }
}

impl From<TimeRangeSpec> for crate::views::time_series::TimeRangeBehavior {
    fn from(range: TimeRangeSpec) -> Self {
        use crate::views::time_series::time_range::Offset;
        Self {
            start: Offset::Expression(range.start),
            end: Offset::Expression(range.end),
        }
    }
}
