//! `mem`: physical memory and swap, in bytes and as percentages.

#[cfg(any(target_os = "linux", test))]
pub mod meminfo;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod os;

use metor_fsw_3::{Frame, JsonSchema, Output, Stop, Timestamp, system};
use serde::Deserialize;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::sample::{Health, Pace};

/// Memory use in bytes. A field the platform does not report reads zero.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(name = "mem")]
#[repr(C)]
pub struct Mem {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    #[frame(unit = "B")]
    pub total: u64,
    #[frame(unit = "B")]
    pub free: u64,
    /// An estimate of what can be allocated without swapping.
    #[frame(unit = "B")]
    pub available: u64,
    /// `total` less `available`.
    #[frame(unit = "B")]
    pub used: u64,
    /// File-backed pages.
    #[frame(unit = "B")]
    pub cached: u64,
    #[frame(unit = "B")]
    pub buffers: u64,
    #[frame(unit = "B")]
    pub active: u64,
    #[frame(unit = "B")]
    pub inactive: u64,
    /// Pages that cannot be paged out.
    #[frame(unit = "B")]
    pub wired: u64,
    #[frame(unit = "B")]
    pub compressed: u64,
    #[frame(unit = "B")]
    pub swap_total: u64,
    #[frame(unit = "B")]
    pub swap_used: u64,
    #[frame(unit = "%")]
    pub used_pct: f64,
    #[frame(unit = "%")]
    pub available_pct: f64,
    #[frame(unit = "%")]
    pub swap_used_pct: f64,
}

impl Mem {
    pub fn zeroed() -> Self {
        Self::new_zeroed()
    }

    /// Fills `used` and the percentages from the byte counts.
    pub fn with_derived(self) -> Self {
        let used = self.total.saturating_sub(self.available);
        Self {
            used,
            used_pct: pct(used, self.total),
            available_pct: pct(self.available, self.total),
            swap_used_pct: pct(self.swap_used, self.swap_total),
            ..self
        }
    }
}

/// Physical memory in bytes, zero when it cannot be read.
pub fn total() -> u64 {
    os::Source::new().read().map_or(0, |mem| mem.total)
}

/// `part` as a percent of `whole`, zero when `whole` is.
pub fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    (100.0 * part as f64 / whole as f64).clamp(0.0, 100.0)
}

/// Configuration for the mem system.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct MemParams {
    /// Samples per second.
    #[serde(default = "default_rate")]
    pub rate_hz: f64,
}

fn default_rate() -> f64 {
    100.0
}

/// The mem system's state.
pub struct MemState {
    pace: Pace,
    health: Health,
    source: os::Source,
}

impl MemState {
    pub fn new(p: MemParams) -> Self {
        Self {
            pace: Pace::new(p.rate_hz),
            health: Health::default(),
            source: os::Source::new(),
        }
    }
}

#[system]
/// Samples memory use `rate_hz` times a second.
pub async fn mem(#[state] state: &mut MemState, mem: &mut Output<Mem>, stop: Stop) {
    while state.pace.wait(&stop).await {
        let read = state.source.read();
        if let Some(sample) = state.health.report("mem", read) {
            let timestamp = Timestamp::now();
            let _ = mem.write(&Mem {
                timestamp,
                ..sample.with_derived()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derived_percentages() {
        let mem = Mem {
            total: 1000,
            available: 250,
            swap_total: 200,
            swap_used: 50,
            ..Mem::zeroed()
        }
        .with_derived();
        assert_eq!(mem.used, 750);
        assert_eq!(mem.used_pct, 75.0);
        assert_eq!(mem.available_pct, 25.0);
        assert_eq!(mem.swap_used_pct, 25.0);
    }

    #[test]
    fn test_no_swap_reads_zero() {
        let mem = Mem {
            total: 1000,
            available: 2000,
            ..Mem::zeroed()
        }
        .with_derived();
        assert_eq!(mem.used, 0);
        assert_eq!(mem.swap_used_pct, 0.0);
        assert_eq!(mem.available_pct, 100.0);
    }
}
