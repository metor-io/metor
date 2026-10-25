//! Pieces every source shares: pacing, reusable file reads, number
//! parsing, and rates.

use std::fs::File;
use std::io::{self, Read};
use std::time::{Duration, Instant};

use metor_fsw_3::Stop;

/// Why a source could not be sampled.
#[derive(Debug, thiserror::Error)]
pub enum SampleError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("malformed {0}")]
    Parse(&'static str),
    #[error("{call} failed with {code}")]
    Os { call: &'static str, code: i32 },
}

pub type Result<T, E = SampleError> = core::result::Result<T, E>;

/// Paces a loop at a fixed rate on the system's thread.
#[derive(Debug)]
pub struct Pace {
    period: Duration,
    next: Option<Instant>,
}

impl Pace {
    /// A pace of `rate_hz` wakes a second, at least one an hour.
    pub fn new(rate_hz: f64) -> Self {
        let period = 1.0 / rate_hz.max(1.0 / 3600.0);
        Self {
            period: Duration::from_secs_f64(period),
            next: None,
        }
    }

    /// Sleeps until the next wake, returning `false` once `stop` is set. The
    /// first call returns at once.
    pub async fn wait(&mut self, stop: &Stop) -> bool {
        let now = Instant::now();
        let next = self
            .next
            .map_or(now, |next| next_wake(next, now, self.period));
        self.next = Some(next);
        let sleep = async {
            stellarator::sleep(next.saturating_duration_since(now)).await;
        };
        futures_lite::future::or(sleep, stop.wait()).await;
        !stop.is_set()
    }
}

/// The wake one `period` after `prev`, or `now` if that has passed.
fn next_wake(prev: Instant, now: Instant, period: Duration) -> Instant {
    (prev + period).max(now)
}

/// Logs a source's first failure, and nothing after until it recovers.
#[derive(Default, Debug)]
pub struct Health {
    failing: bool,
}

impl Health {
    pub fn report<T>(&mut self, source: &str, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => {
                self.failing = false;
                Some(value)
            }
            Err(err) => {
                if !self.failing {
                    tracing::warn!(source, error = %err, "sampling failed");
                }
                self.failing = true;
                None
            }
        }
    }
}

/// A buffer reused across reads of small text files such as those in `/proc`.
#[derive(Debug)]
pub struct TextFile {
    buf: Vec<u8>,
}

impl TextFile {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Reads `path` whole, replacing what the buffer held.
    pub fn read(&mut self, path: &str) -> Result<&str> {
        self.buf.clear();
        File::open(path)?.read_to_end(&mut self.buf)?;
        core::str::from_utf8(&self.buf).map_err(|_| SampleError::Parse("text file"))
    }
}

/// Parses the next whitespace-separated field as a `u64`.
pub fn next_u64<'a>(fields: &mut impl Iterator<Item = &'a str>, what: &'static str) -> Result<u64> {
    fields
        .next()
        .and_then(|f| f.parse().ok())
        .ok_or(SampleError::Parse(what))
}

/// The per-second rate of a counter moving from `prev` to `now` over `dt` s.
pub fn rate(now: u64, prev: u64, dt: f64) -> f64 {
    if dt <= 0.0 {
        return 0.0;
    }
    now.saturating_sub(prev) as f64 / dt
}

/// The seconds between successive samples.
#[derive(Debug)]
pub struct Interval {
    last: Instant,
}

impl Interval {
    pub fn start() -> Self {
        Self {
            last: Instant::now(),
        }
    }

    /// Seconds since the previous call, or since [`start`](Self::start).
    pub fn lap(&mut self) -> f64 {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        dt
    }
}

/// Copies `name` into `out` with every `.` replaced by `_`, so it is one
/// path segment. Returns `None` if the result is empty.
pub fn segment(name: &str, out: &mut String) -> Option<()> {
    out.clear();
    out.extend(name.trim().chars().map(|c| if c == '.' { '_' } else { c }));
    (!out.is_empty()).then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_wake() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(next_wake(t0, t0 + ms(3), ms(10)), t0 + ms(10));
        assert_eq!(next_wake(t0, t0 + ms(25), ms(10)), t0 + ms(25));
        assert_eq!(Pace::new(0.0).period, Duration::from_secs(3600));
    }

    #[test]
    fn test_rate() {
        assert_eq!(rate(300, 100, 2.0), 100.0);
        assert_eq!(rate(100, 300, 2.0), 0.0);
        assert_eq!(rate(300, 100, 0.0), 0.0);
    }

    #[test]
    fn test_next_u64() {
        let mut fields = "12 x".split_ascii_whitespace();
        assert_eq!(next_u64(&mut fields, "n").ok(), Some(12));
        assert!(next_u64(&mut fields, "n").is_err());
        assert!(next_u64(&mut fields, "n").is_err());
    }

    #[test]
    fn test_segment() {
        let mut out = String::new();
        assert_eq!(segment("rx.crc", &mut out), Some(()));
        assert_eq!(out, "rx_crc");
        assert_eq!(segment("  ", &mut out), None);
    }
}
