//! Parses Linux's `/proc/<pid>/stat` and `/proc/<pid>/io`.

use super::ProcStats;
use crate::sample::{Result, SampleError, next_u64};

const STAT: &str = "/proc/<pid>/stat";

/// Units for converting `stat`'s ticks and pages.
#[derive(Clone, Copy, Debug)]
pub struct Units {
    pub tick_ns: u64,
    pub page_bytes: u64,
}

/// Reads CPU time, threads, and RSS. The command name is skipped by its
/// last `)`, since it may hold spaces and parentheses.
pub fn parse_stat(text: &str, units: Units) -> Result<ProcStats> {
    let (_, rest) = text.rsplit_once(')').ok_or(SampleError::Parse(STAT))?;
    // Fields from 3 (`state`) on; utime is field 14.
    let mut fields = rest.split_ascii_whitespace().skip(11);
    let utime = next_u64(&mut fields, STAT)?;
    let stime = next_u64(&mut fields, STAT)?;
    let mut fields = fields.skip(4);
    let threads = next_u64(&mut fields, STAT)?;
    let mut fields = fields.skip(3);
    let rss_pages: i64 = fields
        .next()
        .and_then(|f| f.parse().ok())
        .ok_or(SampleError::Parse(STAT))?;
    let user_ns = utime.saturating_mul(units.tick_ns);
    let system_ns = stime.saturating_mul(units.tick_ns);
    Ok(ProcStats {
        cpu_ns: user_ns.saturating_add(system_ns),
        user_ns,
        system_ns,
        rss_bytes: rss_pages.max(0) as u64 * units.page_bytes,
        threads,
        ..ProcStats::default()
    })
}

/// Reads `read_bytes` and `write_bytes`, the bytes that reached storage.
pub fn parse_io(text: &str) -> Result<(u64, u64)> {
    let (mut read, mut write) = (None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let slot = match key {
            "read_bytes" => &mut read,
            "write_bytes" => &mut write,
            _ => continue,
        };
        *slot = value.trim().parse().ok();
    }
    read.zip(write).ok_or(SampleError::Parse("/proc/<pid>/io"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNITS: Units = Units {
        tick_ns: 10_000_000,
        page_bytes: 4096,
    };

    const STAT_SAMPLE: &str = "4211 (tmux: server (1)) S 1 4211 4211 0 -1 4194624 \
        1406 0 0 0 250 75 0 0 20 0 3 0 1846 9732096 812 18446744073709551615 1 1 0 0 0 0 0 \
        3674112 134433281 0 0 0 17 2 0 0 0 0 0\n";

    #[test]
    fn test_parse_stat_nominal() {
        let stats = parse_stat(STAT_SAMPLE, UNITS).expect("parses");
        assert_eq!(stats.user_ns, 2_500_000_000);
        assert_eq!(stats.system_ns, 750_000_000);
        assert_eq!(stats.cpu_ns, 3_250_000_000);
        assert_eq!(stats.threads, 3);
        assert_eq!(stats.rss_bytes, 812 * 4096);
    }

    #[test]
    fn test_parse_stat_malformed() {
        assert!(parse_stat("", UNITS).is_err());
        assert!(parse_stat("1 (init S 1 2 3", UNITS).is_err());
        assert!(parse_stat("1 (init) S 1 1 1 0 -1 0 0 0 0 0 x 75", UNITS).is_err());
        assert!(parse_stat("1 (init) S 1 1 1 0 -1 0 0 0 0 0 250 75", UNITS).is_err());
    }

    #[test]
    fn test_parse_io() {
        let io = "rchar: 323934931\nwchar: 323929600\nsyscr: 632687\nsyscw: 632675\n\
            read_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_io(io).ok(), Some((4096, 8192)));
        assert!(parse_io("rchar: 1\n").is_err());
        assert!(parse_io("read_bytes: x\nwrite_bytes: 1\n").is_err());
    }
}
