//! Parsers for flags that take units: `10ms`, `100hz`, a UTC time.
//!
//! A bare number keeps the unit the flag had before units: seconds for a
//! duration, hertz for a rate, microseconds since the epoch for a time.

use core::time::Duration;

use jiff::{SignedDuration, Timestamp};

use crate::coordinator::WallRate;

/// Parses `10ms`, `1m30s`, or bare seconds.
pub fn duration(arg: &str) -> Result<Duration, String> {
    let signed = match arg.parse::<f64>() {
        Ok(secs) => SignedDuration::try_from_secs_f64(secs).map_err(|e| e.to_string())?,
        Err(_) => arg
            .parse::<SignedDuration>()
            .map_err(|_| format!("`{arg}` is not a duration like `10ms` or `1.5s`"))?,
    };
    Duration::try_from(signed).map_err(|_| format!("`{arg}` is negative"))
}

/// Parses a duration like [`duration`], refusing zero.
pub fn nonzero_duration(arg: &str) -> Result<Duration, String> {
    match duration(arg)? {
        Duration::ZERO => Err(format!("`{arg}` is zero, or shorter than a nanosecond")),
        span => Ok(span),
    }
}

/// Parses `100hz` or bare hertz.
pub fn rate(arg: &str) -> Result<WallRate, String> {
    let lower = arg.trim().to_ascii_lowercase();
    let number = lower.strip_suffix("hz").unwrap_or(&lower).trim_end();
    let hz: f64 = number
        .parse()
        .map_err(|_| format!("`{arg}` is not a rate like `100hz`"))?;
    WallRate::try_from(hz).map_err(|e| e.to_string())
}

/// Parses an RFC 3339 time such as `2026-10-29T00:00Z`, or bare
/// microseconds since the Unix epoch.
pub fn epoch(arg: &str) -> Result<i64, String> {
    if let Ok(micros) = arg.parse::<i64>() {
        return Ok(micros);
    }
    let at: Timestamp = arg
        .parse()
        .map_err(|_| format!("`{arg}` is not a time like `2026-10-29T00:00Z`"))?;
    Ok(at.as_microsecond())
}

/// How many cycles at `hz` fit in `span`, at least one. Counted from the
/// rate, since a period rounded to nanoseconds can lose a cycle's worth.
pub fn cycles_at(span: Duration, hz: f64) -> u64 {
    let cycles = (span.as_secs_f64() * hz).round();
    if cycles >= u64::MAX as f64 {
        return u64::MAX;
    }
    (cycles as u64).max(1)
}

/// How many cycles of `period` fit in `span`, at least one.
pub fn cycles_in(span: Duration, period: Duration) -> u64 {
    if period.is_zero() {
        return 1;
    }
    let cycles = span.as_nanos().div_ceil(period.as_nanos());
    u64::try_from(cycles).unwrap_or(u64::MAX).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_duration() {
        assert_eq!(duration("10ms"), Ok(Duration::from_millis(10)));
        assert_eq!(duration("1m30s"), Ok(Duration::from_secs(90)));
        assert_eq!(duration("0.01"), Ok(Duration::from_millis(10)));
        assert_eq!(duration("0"), Ok(Duration::ZERO));
        assert!(duration("-1").is_err());
        assert!(duration("-5ms").is_err());
        assert!(duration("fast").is_err());
    }

    #[test]
    fn test_rate() {
        let hz = |arg| rate(arg).map(WallRate::get);
        assert_eq!(hz("100hz"), Ok(100.0));
        assert_eq!(hz("50 Hz"), Ok(50.0));
        assert_eq!(hz("10"), Ok(10.0));
        assert!(rate("0hz").is_err());
        assert!(rate("-3").is_err());
        assert!(rate("quick").is_err());
    }

    #[test]
    fn test_epoch() {
        assert_eq!(epoch("5"), Ok(5));
        assert_eq!(epoch("1970-01-01T00:00:01Z"), Ok(1_000_000));
        assert_eq!(epoch("2026-10-29T00:00Z"), Ok(1_793_232_000_000_000));
        assert!(epoch("yesterday").is_err());
    }

    #[test]
    fn test_nonzero_duration() {
        assert_eq!(nonzero_duration("10ms"), Ok(Duration::from_millis(10)));
        assert!(nonzero_duration("0").is_err());
        assert!(nonzero_duration("0s").is_err());
        assert!(nonzero_duration("1e-10").is_err());
    }

    #[test]
    fn test_cycles_at() {
        let s = Duration::from_secs;
        assert_eq!(cycles_at(s(1), 30.0), 30);
        assert_eq!(cycles_at(s(60), 30.0), 1800);
        assert_eq!(cycles_at(s(1), 1e10), 10_000_000_000);
        assert_eq!(cycles_at(Duration::from_millis(1), 1.0), 1);
        assert_eq!(cycles_at(s(1_000_000), 1e300), u64::MAX);
    }

    #[test]
    fn test_cycles_in() {
        let ms = Duration::from_millis;
        assert_eq!(cycles_in(ms(1000), ms(10)), 100);
        assert_eq!(cycles_in(ms(15), ms(10)), 2);
        assert_eq!(cycles_in(ms(0), ms(10)), 1);
        assert_eq!(cycles_in(ms(5), Duration::ZERO), 1);
    }
}
