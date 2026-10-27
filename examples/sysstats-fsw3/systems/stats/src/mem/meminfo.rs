//! Parses Linux's `/proc/meminfo`.

use super::Mem;
use crate::sample::{Result, SampleError};

const WHAT: &str = "/proc/meminfo";

/// Reads the fields [`Mem`] carries. `MemTotal` must be present.
pub fn parse(text: &str) -> Result<Mem> {
    let mut mem = Mem::zeroed();
    let (mut swap_free, mut seen_total) = (0, false);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let bytes = parse_kb(value)?;
        let slot = match key {
            "MemTotal" => {
                seen_total = true;
                &mut mem.total
            }
            "MemFree" => &mut mem.free,
            "MemAvailable" => &mut mem.available,
            "Cached" => &mut mem.cached,
            "Buffers" => &mut mem.buffers,
            "Active" => &mut mem.active,
            "Inactive" => &mut mem.inactive,
            "Unevictable" => &mut mem.wired,
            "Zswap" => &mut mem.compressed,
            "SwapTotal" => &mut mem.swap_total,
            "SwapFree" => &mut swap_free,
            _ => continue,
        };
        *slot = bytes;
    }
    if !seen_total {
        return Err(SampleError::Parse(WHAT));
    }
    mem.swap_used = mem.swap_total.saturating_sub(swap_free);
    Ok(mem)
}

/// Parses `"  16314732 kB"` into bytes. A value without a unit is a count.
fn parse_kb(value: &str) -> Result<u64> {
    let mut fields = value.split_ascii_whitespace();
    let n: u64 = fields
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or(SampleError::Parse(WHAT))?;
    match fields.next() {
        Some("kB") => Ok(n.saturating_mul(1024)),
        None => Ok(n),
        Some(_) => Err(SampleError::Parse(WHAT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
MemTotal:       16314732 kB
MemFree:         1050236 kB
MemAvailable:    9311744 kB
Buffers:          612404 kB
Cached:          7612344 kB
SwapCached:        11412 kB
Active:          8017232 kB
Inactive:        5492576 kB
Unevictable:        1024 kB
SwapTotal:       2097148 kB
SwapFree:        1048576 kB
HugePages_Total:       0
";

    #[test]
    fn test_parse_nominal() {
        let mem = parse(SAMPLE).expect("parses");
        assert_eq!(mem.total, 16314732 * 1024);
        assert_eq!(mem.available, 9311744 * 1024);
        assert_eq!(mem.cached, 7612344 * 1024);
        assert_eq!(mem.wired, 1024 * 1024);
        assert_eq!(mem.swap_used, (2097148 - 1048576) * 1024);
        assert_eq!(mem.compressed, 0);
    }

    #[test]
    fn test_parse_malformed() {
        assert!(parse("").is_err());
        assert!(parse("MemFree: 12 kB\n").is_err());
        assert!(parse("MemTotal: twelve kB\n").is_err());
        assert!(parse("MemTotal: 12 MB\n").is_err());
    }
}
