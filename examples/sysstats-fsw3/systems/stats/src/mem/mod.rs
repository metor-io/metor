//! `mem`: physical memory and swap, in bytes.

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
    pub total: u64,
    pub free: u64,
    /// An estimate of what can be allocated without swapping.
    pub available: u64,
    /// File-backed pages.
    pub cached: u64,
    pub buffers: u64,
    pub active: u64,
    pub inactive: u64,
    /// Pages that cannot be paged out.
    pub wired: u64,
    pub compressed: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

impl Mem {
    pub fn zeroed() -> Self {
        Self::new_zeroed()
    }
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
                ..sample
            });
        }
    }
}
