//! Decodes the NVMe SMART / Health Information log page (log id `0x02`).

/// Bytes in the log page.
pub const LOG_LEN: usize = 512;

/// A field's name, byte offset, and width in bytes.
const FIELDS: [(&str, usize, usize); 17] = [
    ("critical_warning", 0, 1),
    ("temperature_k", 1, 2),
    ("available_spare", 3, 1),
    ("available_spare_threshold", 4, 1),
    ("percentage_used", 5, 1),
    ("data_units_read", 32, 16),
    ("data_units_written", 48, 16),
    ("host_reads", 64, 16),
    ("host_writes", 80, 16),
    ("controller_busy_minutes", 96, 16),
    ("power_cycles", 112, 16),
    ("power_on_hours", 128, 16),
    ("unsafe_shutdowns", 144, 16),
    ("media_errors", 160, 16),
    ("error_log_entries", 176, 16),
    ("warning_temp_minutes", 192, 4),
    ("critical_temp_minutes", 196, 4),
];

/// Fills `out` with every field of `log`, saturating 128-bit counters.
pub fn parse(log: &[u8; LOG_LEN], out: &mut Vec<(&'static str, u64)>) {
    out.clear();
    out.extend(
        FIELDS
            .iter()
            .map(|&(name, at, len)| (name, le(&log[at..at + len]))),
    );
}

fn le(bytes: &[u8]) -> u64 {
    let mut wide = [0u8; 16];
    wide[..bytes.len()].copy_from_slice(bytes);
    u64::try_from(u128::from_le_bytes(wide)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_log() {
        let mut log = [0u8; LOG_LEN];
        log[1..3].copy_from_slice(&310u16.to_le_bytes());
        log[5] = 3;
        log[32..48].copy_from_slice(&1_234_567u128.to_le_bytes());
        log[128..144].copy_from_slice(&(u128::from(u64::MAX) + 1).to_le_bytes());
        log[196..200].copy_from_slice(&7u32.to_le_bytes());
        let mut out = Vec::new();
        parse(&log, &mut out);
        let get = |name| out.iter().find(|(n, _)| *n == name).map(|(_, v)| *v);
        assert_eq!(out.len(), FIELDS.len());
        assert_eq!(get("temperature_k"), Some(310));
        assert_eq!(get("percentage_used"), Some(3));
        assert_eq!(get("data_units_read"), Some(1_234_567));
        assert_eq!(get("power_on_hours"), Some(u64::MAX));
        assert_eq!(get("critical_temp_minutes"), Some(7));
        assert_eq!(get("media_errors"), Some(0));
    }

    #[test]
    fn test_names_are_keys() {
        for (name, _, _) in FIELDS {
            assert!(metor_component::key_str(name.as_bytes()).is_ok());
        }
    }
}
