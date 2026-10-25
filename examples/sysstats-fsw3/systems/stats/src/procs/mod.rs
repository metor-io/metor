//! `procs`: CPU time, memory, and IO per process, keyed by PID, for the
//! busiest processes.

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod os;
#[cfg(any(target_os = "linux", test))]
pub mod proc_pid;

use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_fsw_3::{Frame, FrameMap, FrameStr, JsonSchema, Output, Stop, Timestamp, system};
use serde::Deserialize;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::sample::{Health, Interval, Pace};

/// One process. IO counters read zero where the OS withholds them.
#[derive(
    AsVTable,
    Metadatatize,
    Componentize,
    Decomponentize,
    IntoBytes,
    Immutable,
    KnownLayout,
    FromBytes,
    Clone,
    Copy,
    Debug,
    PartialEq,
)]
#[repr(C)]
pub struct ProcStats {
    pub cpu_ns: u64,
    pub user_ns: u64,
    pub system_ns: u64,
    pub rss_bytes: u64,
    pub threads: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// CPU time over wall time since the previous sample; 1.0 is one core.
    pub cpu_util: f64,
    /// The command name, cut to 15 bytes.
    pub name: FrameStr<16>,
}

impl Default for ProcStats {
    fn default() -> Self {
        Self {
            cpu_ns: 0,
            user_ns: 0,
            system_ns: 0,
            rss_bytes: 0,
            threads: 0,
            read_bytes: 0,
            write_bytes: 0,
            cpu_util: 0.0,
            name: FrameStr::truncated(""),
        }
    }
}

/// The busiest processes by CPU since the previous sample.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "procs", max_len = 64 * 1024)]
#[repr(C)]
pub struct Procs {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Every process the sample read, before the cut to `max_procs`.
    pub total: u64,
    pub pids: FrameMap<u32, ProcStats>,
}

/// Configuration for the procs system.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ProcsParams {
    /// Samples per second.
    #[serde(default = "default_rate")]
    pub rate_hz: f64,
    /// Processes written at most, the busiest first.
    #[serde(default = "default_max_procs")]
    pub max_procs: u32,
}

fn default_rate() -> f64 {
    10.0
}

fn default_max_procs() -> u32 {
    64
}

/// Processes reserved for at init.
const PROCS: usize = 4096;

/// The procs system's state.
pub struct ProcsState {
    pace: Pace,
    health: Health,
    interval: Interval,
    max: usize,
    source: os::Source,
    now: Vec<(u32, ProcStats)>,
    prev: Vec<(u32, u64)>,
}

impl ProcsState {
    pub fn new(p: ProcsParams) -> Self {
        let mut state = Self {
            pace: Pace::new(p.rate_hz),
            health: Health::default(),
            interval: Interval::start(),
            max: p.max_procs as usize,
            source: os::Source::new(PROCS),
            now: Vec::with_capacity(PROCS),
            prev: Vec::with_capacity(PROCS),
        };
        state.sample();
        state
    }

    fn sample(&mut self) -> Option<()> {
        let read = self.source.read(&mut self.now);
        self.health.report("procs", read)?;
        let dt = self.interval.lap();
        self.now.sort_unstable_by_key(|(pid, _)| *pid);
        rate(&mut self.now, &self.prev, dt);
        self.prev.clear();
        self.prev
            .extend(self.now.iter().map(|(pid, s)| (*pid, s.cpu_ns)));
        Some(())
    }

    fn write(&mut self, now: Timestamp, out: &mut Output<Procs>) {
        let total = self.now.len() as u64;
        let top = busiest(&mut self.now, self.max);
        let fixed = Procs {
            timestamp: now,
            total,
            pids: FrameMap::EMPTY,
        };
        let _ = out.write_with(&fixed, |w| {
            w.map(
                |f| &f.pids,
                top.len(),
                |m| {
                    for (pid, stats) in top.iter() {
                        m.insert(*pid, stats)?;
                    }
                    Ok(())
                },
            )
        });
    }
}

/// Sets each process's `cpu_util` from its CPU time in `prev`, sorted by PID.
/// A process new since `prev` reads zero.
fn rate(now: &mut [(u32, ProcStats)], prev: &[(u32, u64)], dt: f64) {
    for (pid, stats) in now {
        let before = prev
            .binary_search_by_key(pid, |(p, _)| *p)
            .map(|i| prev[i].1);
        stats.cpu_util = match before {
            Ok(before) if dt > 0.0 => stats.cpu_ns.saturating_sub(before) as f64 / 1e9 / dt,
            _ => 0.0,
        };
    }
}

/// Moves the `max` busiest processes to the front, sorted by PID, and
/// returns them.
fn busiest(procs: &mut [(u32, ProcStats)], max: usize) -> &[(u32, ProcStats)] {
    if procs.len() > max && max > 0 {
        procs.select_nth_unstable_by(max - 1, |a, b| b.1.cpu_util.total_cmp(&a.1.cpu_util));
    }
    let n = max.min(procs.len());
    let top = &mut procs[..n];
    top.sort_unstable_by_key(|(pid, _)| *pid);
    top
}

#[system]
/// Samples processes `rate_hz` times a second.
pub async fn procs(#[state] state: &mut ProcsState, procs: &mut Output<Procs>, stop: Stop) {
    while state.pace.wait(&stop).await {
        if state.sample().is_some() {
            state.write(Timestamp::now(), procs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, cpu_ns: u64) -> (u32, ProcStats) {
        let stats = ProcStats {
            cpu_ns,
            ..ProcStats::default()
        };
        (pid, stats)
    }

    #[test]
    fn test_rate_from_previous() {
        let mut now = [proc(1, 3_000_000_000), proc(2, 10), proc(5, 9)];
        rate(&mut now, &[(1, 1_000_000_000), (5, 10)], 2.0);
        assert_eq!(now[0].1.cpu_util, 1.0);
        assert_eq!(now[1].1.cpu_util, 0.0);
        assert_eq!(now[2].1.cpu_util, 0.0);
    }

    #[test]
    fn test_busiest_keeps_top_by_pid() {
        let mut procs: Vec<_> = (1..=6).map(|pid| proc(pid, 0)).collect();
        for (i, (_, s)) in procs.iter_mut().enumerate() {
            s.cpu_util = [0.1, 0.9, 0.0, 0.5, 0.7, 0.2][i];
        }
        let pids: Vec<u32> = busiest(&mut procs, 3).iter().map(|(p, _)| *p).collect();
        assert_eq!(pids, [2, 4, 5]);
        assert_eq!(busiest(&mut procs, 10).len(), 6);
        assert!(busiest(&mut procs, 0).is_empty());
    }
}
