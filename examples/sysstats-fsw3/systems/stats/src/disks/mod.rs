//! `disks`: IO counters per block device, with NVMe SMART health where the
//! platform and permissions allow.

#[cfg(any(target_os = "linux", test))]
pub mod diskstats;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod os;
#[cfg(any(target_os = "linux", test))]
pub mod smart;

use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_fsw_3::{
    Frame, FrameMap, FrameWriteError, JsonSchema, MapWriter, Output, Stop, Timestamp, system,
};
use serde::Deserialize;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::sample::{Health, Interval, Pace, rate};
use crate::set::{Entry, Set};

/// Cumulative device counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub reads: u64,
    pub writes: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ns: u64,
    pub write_ns: u64,
    pub in_flight: u64,
    pub io_ns: u64,
}

/// One device's counters, byte rates (B/s), and SMART health.
#[derive(
    AsVTable,
    Metadatatize,
    Componentize,
    Decomponentize,
    IntoBytes,
    Immutable,
    KnownLayout,
    FromBytes,
    Debug,
)]
#[repr(C)]
pub struct DiskStats {
    pub reads: u64,
    pub writes: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// Time spent on reads (ns).
    pub read_ns: u64,
    /// Time spent on writes (ns).
    pub write_ns: u64,
    pub in_flight: u64,
    /// Time the device had IO in flight (ns).
    pub io_ns: u64,
    pub read_rate: f64,
    pub write_rate: f64,
    /// SMART health by attribute, empty unless the drive answers.
    pub smart: FrameMap<String, u64>,
}

/// Every block device, keyed by name.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "disks", max_len = 64 * 1024)]
#[repr(C)]
pub struct Disks {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub devices: FrameMap<String, DiskStats>,
}

/// Configuration for the disks system.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct DisksParams {
    /// Samples per second.
    #[serde(default = "default_rate")]
    pub rate_hz: f64,
    /// Devices written at most, in the order the platform lists them.
    #[serde(default = "default_max_disks")]
    pub max_disks: u32,
    /// Query NVMe SMART health. Linux only; needs root or `CAP_SYS_ADMIN`.
    #[serde(default = "yes")]
    pub smart: bool,
}

fn default_rate() -> f64 {
    1.0
}

fn yes() -> bool {
    true
}

fn default_max_disks() -> u32 {
    64
}

/// Per-device state beyond its counters.
#[derive(Debug, Default)]
pub struct DiskExtra {
    /// SMART attributes by name, empty when the drive does not answer.
    pub smart: Vec<(&'static str, u64)>,
    pub os: os::Extra,
}

/// The devices seen so far.
pub type DiskSet = Set<Counters, DiskExtra>;

/// One device as last sampled.
pub type Disk = Entry<Counters, DiskExtra>;

fn stats(disk: &Disk, dt: f64) -> DiskStats {
    let (n, p) = (&disk.now, &disk.prev);
    DiskStats {
        reads: n.reads,
        writes: n.writes,
        read_bytes: n.read_bytes,
        write_bytes: n.write_bytes,
        read_ns: n.read_ns,
        write_ns: n.write_ns,
        in_flight: n.in_flight,
        io_ns: n.io_ns,
        read_rate: rate(n.read_bytes, p.read_bytes, dt),
        write_rate: rate(n.write_bytes, p.write_bytes, dt),
        smart: FrameMap::EMPTY,
    }
}

/// The disks system's state.
pub struct DisksState {
    pace: Pace,
    health: Health,
    interval: Interval,
    max_disks: usize,
    source: os::Source,
    set: DiskSet,
}

impl DisksState {
    pub fn new(p: DisksParams) -> Self {
        let max_disks = p.max_disks as usize;
        Self {
            pace: Pace::new(p.rate_hz),
            health: Health::default(),
            interval: Interval::start(),
            max_disks,
            source: os::Source::new(p.smart),
            set: DiskSet::with_capacity(max_disks.max(8) * 2),
        }
    }

    fn sample(&mut self) -> Option<()> {
        self.set.begin();
        let read = self.source.read(&mut self.set);
        self.set.end();
        self.health.report("disks", read)
    }

    fn write(&self, now: Timestamp, dt: f64, out: &mut Output<Disks>) {
        let fixed = Disks {
            timestamp: now,
            devices: FrameMap::EMPTY,
        };
        let count = self.set.len().min(self.max_disks);
        let _ = out.write_with(&fixed, |w| {
            w.map(
                |f| &f.devices,
                count,
                |m| {
                    for disk in self.set.iter().take(count) {
                        insert(m, disk, dt)?;
                    }
                    Ok(())
                },
            )
        });
    }
}

/// Writes one device, skipping it if its key repeats an earlier one.
fn insert(
    m: &mut MapWriter<'_, String, DiskStats>,
    disk: &Disk,
    dt: f64,
) -> Result<(), FrameWriteError> {
    let stats = stats(disk, dt);
    let mut entry = match m.insert(&disk.key, &stats) {
        Err(FrameWriteError::DuplicateKey(_)) => return Ok(()),
        entry => entry?,
    };
    let smart = &disk.extra.smart;
    entry.map(
        |s| &s.smart,
        smart.len(),
        |m| {
            for (name, value) in smart {
                m.insert(name, value)?;
            }
            Ok(())
        },
    )
}

#[system]
/// Samples block device counters `rate_hz` times a second.
pub async fn disks(#[state] state: &mut DisksState, disks: &mut Output<Disks>, stop: Stop) {
    while state.pace.wait(&stop).await {
        let dt = state.interval.lap();
        if state.sample().is_some() {
            state.write(Timestamp::now(), dt, disks);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rates() {
        let mut set = DiskSet::with_capacity(1);
        let at = |read_bytes| Counters {
            read_bytes,
            ..Counters::default()
        };
        set.update("sda", at(4096));
        let disk = set.update("sda", at(8192));
        assert_eq!(stats(disk, 2.0).read_rate, 2048.0);
        assert_eq!(stats(disk, 2.0).read_bytes, 8192);
    }
}
