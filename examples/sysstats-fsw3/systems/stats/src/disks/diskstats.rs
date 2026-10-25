//! Parses Linux's `/proc/diskstats`.

use super::Counters;
use crate::sample::{Result, SampleError, next_u64};

const WHAT: &str = "/proc/diskstats";
const SECTOR: u64 = 512;
const MS: u64 = 1_000_000;

/// Calls `each` with every device's name and counters, skipping loop and
/// RAM devices.
pub fn parse(text: &str, mut each: impl FnMut(&str, Counters)) -> Result<()> {
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut fields = line.split_ascii_whitespace().skip(2);
        let name = fields.next().ok_or(SampleError::Parse(WHAT))?;
        let counters = counters(&mut fields)?;
        if !(name.starts_with("loop") || name.starts_with("ram")) {
            each(name, counters);
        }
    }
    Ok(())
}

/// Reads the eleven columns after the name.
fn counters<'a>(fields: &mut impl Iterator<Item = &'a str>) -> Result<Counters> {
    let mut cols = [0u64; 11];
    for col in &mut cols {
        *col = next_u64(fields, WHAT)?;
    }
    Ok(Counters {
        reads: cols[0],
        read_bytes: cols[2].saturating_mul(SECTOR),
        read_ns: cols[3].saturating_mul(MS),
        writes: cols[4],
        write_bytes: cols[6].saturating_mul(SECTOR),
        write_ns: cols[7].saturating_mul(MS),
        in_flight: cols[8],
        io_ns: cols[9].saturating_mul(MS),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
   7       0 loop0 51 0 2192 9 0 0 0 0 0 28 9 0 0 0 0 0 0
 259       0 nvme0n1 77854 21431 6234970 21880 152213 108412 9834712 145660 2 98020 175644 0 0 0 0 4313 8104
 259       1 nvme0n1p1 312 1211 12806 77 2 0 2 0 0 108 78 0 0 0 0 0 0
   8       0 sda 4588 1240 300218 2187 3045 4127 189264 4589 0 5372 6776
";

    #[test]
    fn test_parse_nominal() {
        let mut seen = Vec::new();
        parse(SAMPLE, |name, c| seen.push((name.to_string(), c))).expect("parses");
        let names: Vec<_> = seen.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["nvme0n1", "nvme0n1p1", "sda"]);
        let nvme = &seen[0].1;
        assert_eq!(nvme.reads, 77854);
        assert_eq!(nvme.read_bytes, 6234970 * 512);
        assert_eq!(nvme.write_ns, 145660 * MS);
        assert_eq!(nvme.in_flight, 2);
        assert_eq!(nvme.io_ns, 98020 * MS);
    }

    #[test]
    fn test_parse_malformed() {
        assert!(parse("8 0 sda 1 2 3\n", |_, _| {}).is_err());
        assert!(parse("8 0\n", |_, _| {}).is_err());
        assert!(parse("8 0 sda 1 2 3 4 5 6 7 8 9 x 11\n", |_, _| {}).is_err());
        assert!(parse("\n", |_, _| {}).is_ok());
    }
}
