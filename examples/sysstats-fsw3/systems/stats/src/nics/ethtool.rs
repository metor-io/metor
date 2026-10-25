//! Decodes the counter names and values the ethtool ioctl returns.

use crate::sample::segment;

/// Bytes per name in an `ETHTOOL_GSTRINGS` reply.
pub const NAME_LEN: usize = 32;

/// Picks up to `max` distinct names from `raw`, an array of NUL-padded
/// 32-byte names. `picks` gets each kept name's index into the stats array.
pub fn pick_names(raw: &[u8], max: usize, names: &mut Vec<String>, picks: &mut Vec<u16>) {
    names.clear();
    picks.clear();
    let mut key = String::new();
    for (i, name) in raw.as_chunks::<NAME_LEN>().0.iter().enumerate() {
        if names.len() == max {
            break;
        }
        let Ok(index) = u16::try_from(i) else {
            break;
        };
        let end = name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        let Ok(name) = core::str::from_utf8(&name[..end]) else {
            continue;
        };
        if segment(name, &mut key).is_none() || names.contains(&key) {
            continue;
        }
        names.push(key.clone());
        picks.push(index);
    }
}

/// Copies the picked counters out of an `ETHTOOL_GSTATS` reply.
pub fn gather(stats: &[u64], picks: &[u16], values: &mut Vec<u64>) {
    values.clear();
    values.extend(
        picks
            .iter()
            .map(|&i| stats.get(usize::from(i)).copied().unwrap_or(0)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(names: &[&[u8]]) -> Vec<u8> {
        let mut raw = vec![0; names.len() * NAME_LEN];
        for (slot, name) in raw.as_chunks_mut::<NAME_LEN>().0.iter_mut().zip(names) {
            slot[..name.len()].copy_from_slice(name);
        }
        raw
    }

    #[test]
    fn test_pick_names() {
        let raw = raw(&[
            b"rx_packets",
            b"tx.queue",
            b"",
            b"rx_packets",
            &[0xff],
            b"tx_drop",
        ]);
        let (mut names, mut picks) = (Vec::new(), Vec::new());
        pick_names(&raw, 8, &mut names, &mut picks);
        assert_eq!(names, ["rx_packets", "tx_queue", "tx_drop"]);
        assert_eq!(picks, [0, 1, 5]);
        pick_names(&raw, 1, &mut names, &mut picks);
        assert_eq!(names, ["rx_packets"]);
    }

    #[test]
    fn test_full_width_name() {
        let raw = [b'a'; NAME_LEN];
        let (mut names, mut picks) = (Vec::new(), Vec::new());
        pick_names(&raw, 8, &mut names, &mut picks);
        assert_eq!(names[0].len(), NAME_LEN);
    }

    #[test]
    fn test_gather() {
        let mut values = Vec::new();
        gather(&[10, 20, 30], &[2, 0, 9], &mut values);
        assert_eq!(values, [30, 10, 0]);
    }
}
