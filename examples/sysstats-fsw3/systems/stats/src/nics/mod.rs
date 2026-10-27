//! `nics`: traffic counters per network interface, with the driver's own
//! counters where the platform exposes them.

#[cfg(any(target_os = "linux", test))]
pub mod ethtool;
#[cfg(any(target_os = "linux", test))]
pub mod net_dev;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod os;

use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_fsw_3::{
    Frame, FrameMap, FrameWriteError, JsonSchema, MapWriter, Output, Stop, Timestamp, system,
};
use serde::Deserialize;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::sample::{Health, Interval, Pace, rate};
use crate::set::{Entry, Set};

/// Cumulative interface counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
}

/// One interface's counters, byte rates (B/s), and driver counters.
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
pub struct NicStats {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
    pub rx_rate: f64,
    pub tx_rate: f64,
    /// The driver's counters by name, from `ethtool -S` on Linux.
    pub ethtool: FrameMap<String, u64>,
}

/// Every interface, keyed by name with `.` replaced by `_`.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "nics", max_len = 128 * 1024)]
#[repr(C)]
pub struct Nics {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub ifaces: FrameMap<String, NicStats>,
}

/// Configuration for the nics system.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct NicsParams {
    /// Samples per second.
    #[serde(default = "default_rate")]
    pub rate_hz: f64,
    /// Interfaces written at most, in the order the platform lists them.
    #[serde(default = "default_max_nics")]
    pub max_nics: u32,
    /// Driver counters written per interface at most.
    #[serde(default = "default_max_ethtool")]
    pub max_ethtool: u32,
}

fn default_rate() -> f64 {
    100.0
}

fn default_max_nics() -> u32 {
    32
}

fn default_max_ethtool() -> u32 {
    64
}

/// Named driver counters, in a fixed order per interface.
#[derive(Debug, Default)]
pub struct DriverCounters {
    pub names: Vec<String>,
    pub values: Vec<u64>,
}

/// Per-interface state beyond its counters.
#[derive(Debug, Default)]
pub struct NicExtra {
    pub driver: DriverCounters,
    pub os: os::Extra,
}

/// The interfaces seen so far.
pub type NicSet = Set<Counters, NicExtra>;

/// One interface as last sampled.
pub type Nic = Entry<Counters, NicExtra>;

fn stats(nic: &Nic, dt: f64) -> NicStats {
    let (n, p) = (&nic.now, &nic.prev);
    NicStats {
        rx_bytes: n.rx_bytes,
        rx_packets: n.rx_packets,
        rx_errors: n.rx_errors,
        rx_dropped: n.rx_dropped,
        tx_bytes: n.tx_bytes,
        tx_packets: n.tx_packets,
        tx_errors: n.tx_errors,
        tx_dropped: n.tx_dropped,
        rx_rate: rate(n.rx_bytes, p.rx_bytes, dt),
        tx_rate: rate(n.tx_bytes, p.tx_bytes, dt),
        ethtool: FrameMap::EMPTY,
    }
}

/// The nics system's state.
pub struct NicsState {
    pace: Pace,
    health: Health,
    interval: Interval,
    max_nics: usize,
    source: os::Source,
    set: NicSet,
}

impl NicsState {
    pub fn new(p: NicsParams) -> Self {
        let max_nics = p.max_nics as usize;
        Self {
            pace: Pace::new(p.rate_hz),
            health: Health::default(),
            interval: Interval::start(),
            max_nics,
            source: os::Source::new(p.max_ethtool as usize),
            set: NicSet::with_capacity(max_nics.max(8) * 2),
        }
    }

    fn sample(&mut self) -> Option<()> {
        self.set.begin();
        let read = self.source.read(&mut self.set);
        self.set.end();
        self.health.report("nics", read)
    }

    fn write(&self, now: Timestamp, dt: f64, out: &mut Output<Nics>) {
        let fixed = Nics {
            timestamp: now,
            ifaces: FrameMap::EMPTY,
        };
        let count = self.set.len().min(self.max_nics);
        let _ = out.write_with(&fixed, |w| {
            w.map(
                |f| &f.ifaces,
                count,
                |m| {
                    for nic in self.set.iter().take(count) {
                        insert(m, nic, dt)?;
                    }
                    Ok(())
                },
            )
        });
    }
}

/// Writes one interface, skipping it if its key repeats an earlier one.
fn insert(
    m: &mut MapWriter<'_, String, NicStats>,
    nic: &Nic,
    dt: f64,
) -> Result<(), FrameWriteError> {
    let stats = stats(nic, dt);
    let mut entry = match m.insert(&nic.key, &stats) {
        Err(FrameWriteError::DuplicateKey(_)) => return Ok(()),
        entry => entry?,
    };
    let driver = &nic.extra.driver;
    entry.map(
        |s| &s.ethtool,
        driver.names.len(),
        |m| {
            for (name, value) in driver.names.iter().zip(&driver.values) {
                m.insert(name, value)?;
            }
            Ok(())
        },
    )
}

#[system]
/// Samples interface counters `rate_hz` times a second.
pub async fn nics(#[state] state: &mut NicsState, nics: &mut Output<Nics>, stop: Stop) {
    while state.pace.wait(&stop).await {
        let dt = state.interval.lap();
        if state.sample().is_some() {
            state.write(Timestamp::now(), dt, nics);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rx(bytes: u64) -> Counters {
        Counters {
            rx_bytes: bytes,
            ..Counters::default()
        }
    }

    #[test]
    fn test_rates() {
        let mut set = NicSet::with_capacity(1);
        set.update("en0", rx(1000));
        let nic = set.update("en0", rx(1300));
        assert_eq!(stats(nic, 1.0).rx_bytes, 1300);
        assert_eq!(stats(nic, 1.5).rx_rate, 200.0);
        let nic = set.update("en1", rx(1000));
        assert_eq!(stats(nic, 1.0).rx_rate, 0.0);
    }
}
