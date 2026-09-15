//! The erased runner and the cycle loop.

use core::future::Future;
use core::pin::pin;
use std::time::Instant;

use futures_lite::future::poll_once;
use metor_proto::types::Timestamp;

use crate::system::System;

use super::Coordinator;
use super::config::Clock;
use super::status::SystemStatus;

/// A bound system, stepped once per cycle with the system type erased.
pub trait Step {
    fn execute(&mut self, now: Timestamp);
}

pub(crate) struct Runner<S: System> {
    pub system: S,
    pub state: S::State,
    pub inputs: S::Inputs,
    pub outputs: S::Outputs,
}

impl<S: System> Step for Runner<S> {
    fn execute(&mut self, _now: Timestamp) {
        self.system
            .execute(&mut self.state, &mut self.inputs, &mut self.outputs);
    }
}

impl Coordinator {
    /// Cycles completed so far.
    pub fn cycle(&self) -> u64 {
        self.cycle
    }

    /// Execute every system once, in list order, publishing each one's timing.
    ///
    /// A status write that fails is dropped: nothing in a cycle fails.
    pub fn step(&mut self, now: Timestamp) {
        let cycle_start = Instant::now();
        for entry in &mut self.entries {
            let started = Instant::now();
            entry.step.execute(now);
            let _ = entry.status.write(&SystemStatus {
                timestamp: now,
                exec_time_ns: nanos(started.elapsed()),
                exec_offset_ns: nanos(started.duration_since(cycle_start)),
            });
        }
        self.cycle += 1;
    }

    /// Step until `stop` resolves, which is checked once per cycle.
    ///
    /// Under a wall clock the loop sleeps out the remainder of the cycle
    /// budget and yields instead when a cycle overruns; under a simulated
    /// clock it only yields, so cycles run as fast as the host allows.
    pub async fn run(&mut self, stop: impl Future<Output = ()>) {
        let mut stop = pin!(stop);
        let budget = self.clock.budget();
        loop {
            let started = Instant::now();
            let now = self.now();
            self.step(now);
            match started.elapsed() {
                elapsed if elapsed < budget => stellarator::sleep(budget - elapsed).await,
                _ => stellarator::yield_now().await,
            }
            if poll_once(stop.as_mut()).await.is_some() {
                return;
            }
        }
    }

    /// This cycle's timestamp.
    fn now(&self) -> Timestamp {
        match self.clock {
            Clock::Wall { .. } => Timestamp::now(),
            Clock::Simulated { dt } => simulated(self.epoch, self.cycle, dt),
        }
    }
}

/// `epoch + cycle * dt`, saturating rather than wrapping at the ends of the
/// microsecond range.
fn simulated(epoch: Timestamp, cycle: u64, dt: core::time::Duration) -> Timestamp {
    let nanos = dt.as_nanos().saturating_mul(u128::from(cycle));
    let micros = i64::try_from(nanos / 1_000).unwrap_or(i64::MAX);
    Timestamp(epoch.0.saturating_add(micros))
}

fn nanos(elapsed: core::time::Duration) -> u64 {
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;
    use crate::coordinator::CoordinatorConfig;
    use crate::coordinator::config::{InputConfig, PortRef, SystemConfig};
    use crate::coordinator::fixtures::{self, Recorder, pipeline_config, table};

    /// `imu -> nav -> control` plus a system reading the two producers' status
    /// rings.
    fn watched(config: &mut CoordinatorConfig, from: &[&str]) {
        config.systems.push(SystemConfig {
            id: "watch".into(),
            ty: "status_watch".into(),
            inputs: vec![InputConfig {
                port: "status".into(),
                from: from.iter().map(|id| PortRef::new(*id, "status")).collect(),
            }],
        });
    }

    #[test]
    fn a_pipeline_flows_within_one_cycle() {
        let recorder = Recorder::default();
        let mut coordinator = fixtures::build(pipeline_config(), &table(&recorder));
        coordinator.step(Timestamp(10));
        assert_eq!(recorder.take(), vec![(Timestamp(1), 3.0)]);
        assert_eq!(coordinator.cycle(), 1);
    }

    #[test]
    fn a_consumer_before_its_producer_reads_the_previous_cycle() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.systems.swap(0, 1);
        let mut coordinator = fixtures::build(config, &table(&recorder));
        coordinator.step(Timestamp(1));
        assert_eq!(recorder.take(), Vec::new());
        coordinator.step(Timestamp(2));
        assert_eq!(recorder.take(), vec![(Timestamp(1), 3.0)]);
    }

    #[test]
    fn fan_in_takes_the_newer_producer() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.systems[1].inputs[0]
            .from
            .push(PortRef::new("imu_late", "imu"));
        config.systems.insert(
            1,
            SystemConfig {
                id: "imu_late".into(),
                ty: "imu_offset".into(),
                inputs: Vec::new(),
            },
        );
        let mut coordinator = fixtures::build(config, &table(&recorder));
        coordinator.step(Timestamp(5));
        // `imu_offset` stamps 100 later and samples ten times higher.
        assert_eq!(recorder.take(), vec![(Timestamp(101), 21.0)]);
    }

    #[test]
    fn status_carries_the_cycle_and_a_rising_offset() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        watched(&mut config, &["imu", "nav"]);
        let mut coordinator = fixtures::build(config, &table(&recorder));
        coordinator.step(Timestamp(99));

        let seen = recorder.take_status();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|s| s.timestamp == Timestamp(99)));
        assert!(seen[0].exec_offset_ns <= seen[1].exec_offset_ns);
        // `nav` spins, so its execution outlasts the clock's resolution.
        assert!(seen[1].exec_time_ns > 0);
    }

    #[stellarator::test]
    async fn run_stops_on_the_stop_future() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.clock = Clock::Wall { rate: 10_000.0 };
        let mut coordinator = fixtures::build(config, &table(&recorder));
        coordinator.run(fixtures::after_cycles(5)).await;
        assert_eq!(coordinator.cycle(), 5);
        assert_eq!(recorder.take().len(), 5);
    }

    #[stellarator::test]
    async fn a_simulated_clock_advances_by_dt() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.clock = Clock::Simulated {
            dt: Duration::from_millis(20),
        };
        watched(&mut config, &["imu"]);
        let mut coordinator = fixtures::build(config, &table(&recorder));
        let epoch = coordinator.epoch.0;
        coordinator.run(fixtures::after_cycles(3)).await;

        let stamps: Vec<_> = recorder
            .take_status()
            .into_iter()
            .map(|status| status.timestamp.0 - epoch)
            .collect();
        assert_eq!(stamps, vec![0, 20_000, 40_000]);
    }

    #[test]
    fn an_empty_coordinator_still_counts_cycles() {
        let mut coordinator =
            fixtures::build(CoordinatorConfig::default(), &table(&Recorder::default()));
        coordinator.step(Timestamp(0));
        assert_eq!(coordinator.cycle(), 1);
    }

    #[test]
    fn a_simulated_timestamp_saturates_instead_of_wrapping() {
        let far = simulated(Timestamp(i64::MAX), u64::MAX, Duration::from_secs(1));
        assert_eq!(far, Timestamp(i64::MAX));
        assert_eq!(
            simulated(Timestamp(5), 0, Duration::from_secs(1)),
            Timestamp(5)
        );
    }
}
