//! Parses Linux's `/proc/net/dev`.

use super::Counters;
use crate::sample::{Result, SampleError, next_u64};

const WHAT: &str = "/proc/net/dev";

/// Calls `each` with every interface's name and counters.
pub fn parse(text: &str, mut each: impl FnMut(&str, Counters)) -> Result<()> {
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(SampleError::Parse(WHAT));
        }
        each(name, counters(rest)?);
    }
    Ok(())
}

/// Reads the eight receive and eight transmit columns.
fn counters(rest: &str) -> Result<Counters> {
    let mut cols = [0u64; 16];
    let mut fields = rest.split_ascii_whitespace();
    for col in &mut cols {
        *col = next_u64(&mut fields, WHAT)?;
    }
    Ok(Counters {
        rx_bytes: cols[0],
        rx_packets: cols[1],
        rx_errors: cols[2],
        rx_dropped: cols[3],
        tx_bytes: cols[8],
        tx_packets: cols[9],
        tx_errors: cols[10],
        tx_dropped: cols[11],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 2776770   11307    0    0    0     0          0         0  2776770   11307    0    0    0     0       0          0
  eth0:1215645    2751    1    2    0     0          0         0  1782404    4324    3    4    0   427       0          0
";

    #[test]
    fn test_parse_nominal() {
        let mut seen = Vec::new();
        parse(SAMPLE, |name, c| seen.push((name.to_string(), c))).expect("parses");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, "lo");
        let (name, eth0) = &seen[1];
        assert_eq!(name, "eth0");
        assert_eq!(eth0.rx_bytes, 1215645);
        assert_eq!(eth0.rx_dropped, 2);
        assert_eq!(eth0.tx_bytes, 1782404);
        assert_eq!(eth0.tx_dropped, 4);
    }

    #[test]
    fn test_parse_malformed() {
        assert!(parse("eth0: 1 2 3\n", |_, _| {}).is_err());
        assert!(parse(": 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n", |_, _| {}).is_err());
        assert!(parse("eth0: 1 2 3 4 5 6 7 8 9 10 11 x 13 14 15 16\n", |_, _| {}).is_err());
        assert!(parse("", |_, _| {}).is_ok());
    }
}
