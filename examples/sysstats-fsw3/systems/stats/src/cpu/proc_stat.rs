//! Parses the `cpu` lines of Linux's `/proc/stat`.

use super::Ticks;
use crate::sample::{Result, SampleError, next_u64};

/// Fills `total` from the `cpu` line and `cores` from each `cpuN` line.
pub fn parse(text: &str, total: &mut Ticks, cores: &mut Vec<(u16, Ticks)>) -> Result<()> {
    cores.clear();
    let mut seen_total = false;
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(label) = fields.next().and_then(|l| l.strip_prefix("cpu")) else {
            continue;
        };
        let ticks = parse_ticks(&mut fields)?;
        if label.is_empty() {
            *total = ticks;
            seen_total = true;
            continue;
        }
        let id = label
            .parse()
            .map_err(|_| SampleError::Parse("/proc/stat"))?;
        cores.push((id, ticks));
    }
    seen_total
        .then_some(())
        .ok_or(SampleError::Parse("/proc/stat"))
}

/// Reads the tick columns after a `cpu` label. Kernels before 2.6.11 omit
/// the trailing columns, which read as zero.
fn parse_ticks<'a>(fields: &mut impl Iterator<Item = &'a str>) -> Result<Ticks> {
    let what = "/proc/stat";
    let mut ticks = Ticks {
        user: next_u64(fields, what)?,
        nice: next_u64(fields, what)?,
        system: next_u64(fields, what)?,
        idle: next_u64(fields, what)?,
        ..Ticks::default()
    };
    let rest = [
        &mut ticks.iowait,
        &mut ticks.irq,
        &mut ticks.softirq,
        &mut ticks.steal,
    ];
    for slot in rest {
        match fields.next() {
            Some(f) => *slot = f.parse().map_err(|_| SampleError::Parse(what))?,
            None => break,
        }
    }
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
cpu  2255 34 2290 22625563 6290 127 456 7 0 0
cpu0 1132 34 1441 11311718 3675 127 438 3 0 0
cpu1 1123 0 849 11313845 2614 0 18 4 0 0
intr 114930548 113199788 3 0 5 263 0 4 [... lots more numbers ...]
ctxt 1990473
btime 1062191376
";

    #[test]
    fn test_parse_nominal() {
        let (mut total, mut cores) = (Ticks::default(), Vec::new());
        parse(SAMPLE, &mut total, &mut cores).expect("parses");
        assert_eq!(total.user, 2255);
        assert_eq!(total.idle, 22625563);
        assert_eq!(total.steal, 7);
        assert_eq!(cores.len(), 2);
        assert_eq!(cores[1].0, 1);
        assert_eq!(cores[1].1.system, 849);
    }

    #[test]
    fn test_parse_short_rows() {
        let (mut total, mut cores) = (Ticks::default(), Vec::new());
        parse("cpu 1 2 3 4\ncpu3 1 2 3 4\n", &mut total, &mut cores).expect("parses");
        assert_eq!(total.idle, 4);
        assert_eq!(total.iowait, 0);
        assert_eq!(cores[0].0, 3);
    }

    #[test]
    fn test_parse_malformed() {
        let (mut total, mut cores) = (Ticks::default(), Vec::new());
        assert!(parse("", &mut total, &mut cores).is_err());
        assert!(parse("cpu 1 2 x 4\n", &mut total, &mut cores).is_err());
        assert!(parse("cpu 1 2 3\n", &mut total, &mut cores).is_err());
        assert!(parse("cpu 1 2 3 4\ncpuX 1 2 3 4\n", &mut total, &mut cores).is_err());
    }
}
