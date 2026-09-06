use super::{Anchor, TemporalConfig, TimeContext, TimeExpr, TimeRangeSpec, model};
use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};

/// Presentation of selected time; changing it never changes the selected instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeDisplay {
    #[default]
    Timestamp,
    Elapsed,
}

impl TimeDisplay {
    pub fn label(self) -> &'static str {
        match self {
            Self::Timestamp => "timestamp",
            Self::Elapsed => "elapsed / T0",
        }
    }
}

pub(crate) fn origin(config: &TemporalConfig, context: &TimeContext) -> Option<Timestamp> {
    config
        .t0
        .map(Timestamp)
        .or_else(|| context.extent.as_ref().map(|r| r.start))
}

/// Compact signed offset shared by temporal controls and plot axes.
pub(crate) fn elapsed(t: Timestamp, zero: Timestamp) -> String {
    let delta = i128::from(t.0) - i128::from(zero.0);
    let millis = delta.unsigned_abs() / 1_000;
    if millis == 0 {
        return "T+0".into();
    }
    let seconds = millis / 1_000;
    let fraction = millis % 1_000;
    let time = if seconds < 60 {
        if fraction == 0 {
            format!("{seconds:02}")
        } else {
            format!("{seconds}.{fraction:03}s")
        }
    } else {
        let mut time = if seconds < 3600 {
            format!("{:02}:{:02}", seconds / 60, seconds % 60)
        } else {
            format!(
                "{:02}:{:02}:{:02}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            )
        };
        if fraction != 0 {
            time.push_str(&format!(".{fraction:03}"));
        }
        time
    };
    format!("T{}{time}", if delta < 0 { '-' } else { '+' })
}

pub(crate) fn timestamp(t: Timestamp, config: &TemporalConfig, context: &TimeContext) -> String {
    if config.display == TimeDisplay::Elapsed {
        let Some(zero) = origin(config, context) else {
            return "T0 unavailable".into();
        };
        return elapsed(t, zero);
    }
    let zone = if config.timezone.eq_ignore_ascii_case("local") {
        jiff::tz::TimeZone::system()
    } else {
        jiff::tz::TimeZone::get(&config.timezone).unwrap_or(jiff::tz::TimeZone::UTC)
    };
    jiff::Timestamp::from_microsecond(t.0)
        .map(|t| {
            t.to_zoned(zone)
                .strftime("%Y-%m-%d %H:%M:%S%.3f %Z")
                .to_string()
        })
        .unwrap_or_else(|_| model::timestamp_text(t, &config.timezone))
}

pub(crate) fn label(t: Timestamp, cx: &gpui::App) -> String {
    super::snapshot(cx).map_or_else(
        || model::timestamp_text(t, "UTC"),
        |s| timestamp(t, &super::config(cx), &s.context),
    )
}

pub(crate) fn range(
    range: TimeRangeSpec,
    config: &TemporalConfig,
    context: &TimeContext,
) -> String {
    let endpoint = |expr: TimeExpr| {
        if matches!(expr.anchor, Anchor::Timestamp(_)) {
            expr.resolve(context)
                .map(|t| timestamp(t, config, context))
                .unwrap_or_else(|_| expr.to_string())
        } else {
            expr.to_string()
        }
    };
    if matches!(range.start.anchor, Anchor::Timestamp(_))
        || matches!(range.end.anchor, Anchor::Timestamp(_))
    {
        format!("{} → {}", endpoint(range.start), endpoint(range.end))
    } else {
        range.to_string()
    }
}

pub(crate) fn expand_input(
    query: &str,
    config: &TemporalConfig,
    context: &TimeContext,
) -> Result<String, String> {
    let instant = |text: &str| -> Result<String, String> {
        let lower = text.trim().to_ascii_lowercase();
        let suffix = if let Some(s) = lower.strip_prefix("t0") {
            s.trim()
        } else if lower.starts_with("t+") || lower.starts_with("t-") {
            &lower[1..]
        } else {
            return Ok(text.to_string());
        };
        let zero =
            origin(config, context).ok_or("T0 unavailable; choose a T0 timestamp or load data")?;
        let (negative, duration) = if suffix.is_empty() {
            (false, "0s")
        } else if let Some(s) = suffix.strip_prefix('+') {
            (false, s.trim())
        } else if let Some(s) = suffix.strip_prefix('-') {
            (true, s.trim())
        } else {
            return Err("Use T0 + 5m, T-30s, or T+00:05:00".into());
        };
        let duration = if duration.contains(':') {
            let parts: Vec<_> = duration.split(':').collect();
            if !(2..=3).contains(&parts.len()) {
                return Err("Use T+MM:SS, T+HH:MM:SS, or a duration such as T+2.5m".into());
            }
            let (hours, minutes, seconds) = if parts.len() == 3 {
                (parts[0], parts[1], parts[2])
            } else {
                ("0", parts[0], parts[1])
            };
            if [hours, minutes]
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
                || seconds.is_empty()
                || !seconds.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            {
                return Err("Use numeric hours, minutes, and seconds".into());
            }
            let minutes: u64 = minutes.parse().map_err(|_| "Invalid minutes")?;
            let seconds = model::duration(&format!("{seconds}s"))?;
            if minutes >= 60 || seconds >= 60_000_000 {
                return Err("Minutes and seconds must be below 60".into());
            }
            model::duration(&format!("{hours}h {minutes}m"))?
                .checked_add(seconds)
                .ok_or("Duration overflow")?
        } else if !duration.is_empty() && duration.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        {
            model::duration(&format!("{duration}s"))?
        } else {
            model::duration(duration)?
        };
        let offset = if negative { -duration } else { duration };
        let t = zero.0.checked_add(offset).ok_or("Timestamp overflow")?;
        Ok(model::timestamp_text(Timestamp(t), "UTC"))
    };
    if let Some((start, end)) = query.split_once("..").or_else(|| query.split_once('↔')) {
        Ok(format!("{} .. {}", instant(start)?, instant(end)?))
    } else {
        instant(query)
    }
}
