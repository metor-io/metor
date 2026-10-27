//! `cpu`: cumulative CPU time per core and in total, with smoothed
//! percentages of time by state.

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod os;
#[cfg(any(target_os = "linux", test))]
pub mod proc_stat;

use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_fsw_3::{Frame, FrameMap, JsonSchema, Output, Stop, Timestamp, system};
use serde::Deserialize;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::sample::{Health, Interval, Pace};

/// Cumulative CPU time in clock ticks, by state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ticks {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl Ticks {
    fn total(&self) -> u64 {
        self.busy() + self.idle + self.iowait
    }

    fn busy(&self) -> u64 {
        self.user + self.nice + self.system + self.irq + self.softirq + self.steal
    }

    /// Ticks accrued since `prev`, by state.
    fn since(&self, prev: &Ticks) -> Ticks {
        Ticks {
            user: self.user.saturating_sub(prev.user),
            nice: self.nice.saturating_sub(prev.nice),
            system: self.system.saturating_sub(prev.system),
            idle: self.idle.saturating_sub(prev.idle),
            iowait: self.iowait.saturating_sub(prev.iowait),
            irq: self.irq.saturating_sub(prev.irq),
            softirq: self.softirq.saturating_sub(prev.softirq),
            steal: self.steal.saturating_sub(prev.steal),
        }
    }

    /// Busy, user, system, iowait, and idle ticks, the shares [`Load`] tracks.
    fn shares(&self) -> [u64; SHARES] {
        [
            self.busy(),
            self.user + self.nice,
            self.system + self.irq + self.softirq,
            self.iowait,
            self.idle,
        ]
    }
}

const SHARES: usize = 5;

/// One core's cumulative ticks and the percent of its time in each state.
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
    Default,
)]
#[repr(C)]
pub struct CoreStats {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    /// Time not idle.
    #[fsw(unit = "%")]
    pub usage_pct: f64,
    /// User code, `nice` included.
    #[fsw(unit = "%")]
    pub user_pct: f64,
    /// Kernel code and interrupts.
    #[fsw(unit = "%")]
    pub system_pct: f64,
    /// Idle waiting on IO.
    #[fsw(unit = "%")]
    pub iowait_pct: f64,
    #[fsw(unit = "%")]
    pub idle_pct: f64,
}

/// All cores summed, and each core keyed by its index.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "cpu", max_len = 32 * 1024)]
#[repr(C)]
pub struct Cpu {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    #[frame(nest)]
    pub total: CoreStats,
    pub cores: FrameMap<u16, CoreStats>,
}

/// Configuration for the cpu system.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct CpuParams {
    /// Samples per second.
    #[serde(default = "default_rate")]
    pub rate_hz: f64,
    /// Time constant (s) of the percentage averages.
    #[serde(default = "default_tau")]
    pub util_tau_s: f64,
}

fn default_rate() -> f64 {
    100.0
}

fn default_tau() -> f64 {
    1.0
}

/// Exponential averages of each share's ticks and the total per sample.
#[derive(Clone, Copy, Debug, Default)]
struct Load {
    shares: [f64; SHARES],
    total: f64,
}

impl Load {
    fn update(&mut self, delta: Ticks, alpha: f64) {
        for (avg, ticks) in self.shares.iter_mut().zip(delta.shares()) {
            *avg += alpha * (ticks as f64 - *avg);
        }
        self.total += alpha * (delta.total() as f64 - self.total);
    }

    /// Each share as a percent of the total.
    fn pct(&self) -> [f64; SHARES] {
        if self.total <= 0.0 {
            return [0.0; SHARES];
        }
        self.shares
            .map(|share| (100.0 * share / self.total).clamp(0.0, 100.0))
    }
}

/// One core's last ticks and load.
#[derive(Clone, Copy, Debug, Default)]
struct Track {
    id: u16,
    ticks: Ticks,
    load: Load,
}

impl Track {
    fn new(id: u16, ticks: Ticks) -> Self {
        Self {
            id,
            ticks,
            load: Load::default(),
        }
    }

    fn update(&mut self, now: Ticks, alpha: f64) {
        self.load.update(now.since(&self.ticks), alpha);
        self.ticks = now;
    }

    fn stats(&self) -> CoreStats {
        let t = &self.ticks;
        let [usage_pct, user_pct, system_pct, iowait_pct, idle_pct] = self.load.pct();
        CoreStats {
            user: t.user,
            nice: t.nice,
            system: t.system,
            idle: t.idle,
            iowait: t.iowait,
            irq: t.irq,
            softirq: t.softirq,
            steal: t.steal,
            usage_pct,
            user_pct,
            system_pct,
            iowait_pct,
            idle_pct,
        }
    }
}

/// The cpu system's state.
pub struct CpuState {
    pace: Pace,
    health: Health,
    interval: Interval,
    tau: f64,
    source: os::Source,
    total: Track,
    cores: Vec<Track>,
    read: Vec<(u16, Ticks)>,
}

/// Cores reserved for at init.
const CORES: usize = 256;

impl CpuState {
    pub fn new(p: CpuParams) -> Self {
        let mut state = Self {
            pace: Pace::new(p.rate_hz),
            health: Health::default(),
            interval: Interval::start(),
            tau: p.util_tau_s.max(1e-3),
            source: os::Source::new(),
            total: Track::default(),
            cores: Vec::with_capacity(CORES),
            read: Vec::with_capacity(CORES),
        };
        state.sample();
        state
    }

    fn sample(&mut self) -> Option<()> {
        let mut total = Ticks::default();
        let read = self.source.read(&mut total, &mut self.read);
        self.health.report("cpu", read)?;
        let alpha = 1.0 - (-self.interval.lap() / self.tau).exp();
        self.total.update(total, alpha);
        self.track(alpha);
        Some(())
    }

    /// Updates each core's track from `read`, starting over if the set of
    /// cores changed.
    fn track(&mut self, alpha: f64) {
        let same = self.cores.len() == self.read.len()
            && self
                .cores
                .iter()
                .zip(&self.read)
                .all(|(t, (id, _))| t.id == *id);
        if !same {
            self.cores.clear();
            self.cores
                .extend(self.read.iter().map(|&(id, ticks)| Track::new(id, ticks)));
            return;
        }
        for (track, (_, ticks)) in self.cores.iter_mut().zip(&self.read) {
            track.update(*ticks, alpha);
        }
    }

    fn write(&self, now: Timestamp, out: &mut Output<Cpu>) {
        let fixed = Cpu {
            timestamp: now,
            total: self.total.stats(),
            cores: FrameMap::EMPTY,
        };
        let _ = out.write_with(&fixed, |w| {
            w.map(
                |f| &f.cores,
                self.cores.len(),
                |m| {
                    for track in &self.cores {
                        m.insert(track.id, &track.stats())?;
                    }
                    Ok(())
                },
            )
        });
    }
}

#[system]
/// Samples CPU time `rate_hz` times a second.
pub async fn cpu(#[state] state: &mut CpuState, cpu: &mut Output<Cpu>, stop: Stop) {
    while state.pace.wait(&stop).await {
        if state.sample().is_some() {
            state.write(Timestamp::now(), cpu);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticks(user: u64, idle: u64) -> Ticks {
        Ticks {
            user,
            idle,
            ..Ticks::default()
        }
    }

    #[test]
    fn test_track_utilization() {
        let mut track = Track::new(0, ticks(0, 0));
        for i in 1..=200 {
            track.update(ticks(i * 3, i), 0.1);
        }
        let stats = track.stats();
        assert!((stats.usage_pct - 75.0).abs() < 1e-6);
        assert!((stats.user_pct - 75.0).abs() < 1e-6);
        assert!((stats.idle_pct - 25.0).abs() < 1e-6);
        assert_eq!(stats.system_pct, 0.0);
        assert_eq!(stats.user, 600);
    }

    #[test]
    fn test_counter_reset_reads_idle() {
        let mut track = Track::new(0, ticks(100, 100));
        track.update(ticks(0, 0), 1.0);
        assert_eq!(track.stats().usage_pct, 0.0);
    }

    #[test]
    fn test_layout() {
        assert_eq!(FrameMap::<u16, CoreStats>::STRIDE, 112);
    }
}
