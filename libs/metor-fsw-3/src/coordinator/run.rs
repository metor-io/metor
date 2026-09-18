//! The erased runner and the cycle loop.

use core::any::Any;
use core::future::Future;
use core::pin::pin;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use futures_lite::future::poll_once;
use metor_proto::types::Timestamp;

use crate::system::System;

use super::Coordinator;
use super::config::Clock;
use super::status::SystemStatus;

/// A bound system, stepped once per cycle with the system type erased.
pub trait Step {
    fn execute(&mut self, now: Timestamp);

    /// Called once after `execute` panicked, with the payload's message.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    /// Whether the step latched itself off, as one across an ABI does.
    fn latched(&self) -> bool {
        false
    }
}

pub(crate) struct Runner<S: System> {
    pub system: S,
    pub state: S::State,
    pub inputs: S::Inputs,
    pub outputs: S::Outputs,
}

impl<S: System> Step for Runner<S> {
    fn execute(&mut self, now: Timestamp) {
        self.system
            .execute(now, &mut self.state, &mut self.inputs, &mut self.outputs);
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.system.fault(now, &mut self.outputs, message);
    }
}

/// Executes once, reporting a fault and retiring the runner on failure.
pub(crate) fn catch_step(slot: &mut Option<Box<dyn Step>>, now: Timestamp) -> bool {
    let Some(step) = slot.as_mut() else {
        return false;
    };
    // PANIC Safety: a failed step is faulted once and removed before returning.
    let result = catch_unwind(AssertUnwindSafe(|| {
        if step.latched() {
            return false;
        }
        step.execute(now);
        !step.latched()
    }));
    let healthy = match result {
        Ok(healthy) => healthy,
        Err(payload) => {
            crate::panic::catch(|| step.fault(now, message_of(&*payload)));
            crate::panic::discard(payload);
            false
        }
    };
    if !healthy {
        crate::panic::catch(|| drop(slot.take()));
    }
    healthy
}

pub(crate) fn message_of(payload: &(dyn Any + Send)) -> &str {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return text;
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text;
    }
    "panic"
}

impl Coordinator {
    /// Cycles completed so far.
    pub fn cycle(&self) -> u64 {
        self.cycle
    }

    /// Execute a single step of the coordinator, running each system once.
    ///
    /// A system that panicked on an earlier cycle is skipped; its status keeps
    /// arriving with a zero execution time.
    pub fn step(&mut self, now: Timestamp) {
        let cycle_start = Instant::now();
        for entry in &mut self.entries {
            let offset = cycle_start.elapsed();
            let elapsed = entry.run(now);
            let _ = entry.status.write(&SystemStatus {
                timestamp: now,
                exec_time_ns: duration_to_nanos(elapsed),
                exec_offset_ns: duration_to_nanos(offset),
            });
        }
        self.cycle += 1;
    }

    /// Step until `stop` is ready
    pub async fn run(&mut self, stop: impl Future<Output = ()>) {
        let mut stop = pin!(stop);
        let budget = self.clock.cycle_budget();
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
            Clock::Simulated { dt } => simulated_time(self.epoch, self.cycle, dt),
        }
    }
}

impl super::Entry {
    pub(super) fn latched(&self) -> bool {
        self.step.is_none()
    }

    fn run(&mut self, now: Timestamp) -> Duration {
        if self.latched() {
            return Duration::ZERO;
        }
        let started = Instant::now();
        let healthy = catch_step(&mut self.step, now);
        let elapsed = started.elapsed();
        if !healthy {
            crate::panic::catch(|| tracing::error!(system = self.name, "system panicked"));
        }
        elapsed
    }
}

/// Calculates the simulation time from an epoch, the current cycle, and dt.
fn simulated_time(epoch: Timestamp, cycle: u64, dt: core::time::Duration) -> Timestamp {
    let nanos = dt.as_nanos().saturating_mul(u128::from(cycle));
    let micros = i64::try_from(nanos / 1_000).unwrap_or(i64::MAX);
    Timestamp(epoch.0.saturating_add(micros))
}

fn duration_to_nanos(elapsed: core::time::Duration) -> u64 {
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;
    use crate::coordinator::CoordinatorConfig;
    use crate::coordinator::config::{InputConfig, PortRef, SystemConfig};
    use crate::tests::utils::{self, Recorder, pipeline_config, table};

    fn watched(config: &mut CoordinatorConfig, from: &[&str]) {
        config.systems.push(SystemConfig {
            inputs: vec![InputConfig {
                port: "status".into(),
                from: from.iter().map(|id| PortRef::new(*id, "status")).collect(),
            }],
            ..SystemConfig::new("watch", "status_watch")
        });
    }

    #[test]
    fn a_pipeline_flows_within_one_cycle() {
        let recorder = Recorder::default();
        let mut coordinator = pipeline_config().build(&table(&recorder)).unwrap();
        coordinator.step(Timestamp(10));
        assert_eq!(recorder.take(), vec![(Timestamp(1), 3.0)]);
        assert_eq!(coordinator.cycle(), 1);
    }

    #[test]
    fn a_consumer_before_its_producer_reads_the_previous_cycle() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.systems.swap(0, 1);
        let mut coordinator = config.build(&table(&recorder)).unwrap();
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
        config
            .systems
            .insert(1, SystemConfig::new("imu_late", "imu_offset"));
        let mut coordinator = config.build(&table(&recorder)).unwrap();
        coordinator.step(Timestamp(5));
        // `imu_offset` stamps 100 later and samples ten times higher.
        assert_eq!(recorder.take(), vec![(Timestamp(101), 21.0)]);
    }

    #[test]
    fn status_carries_the_cycle_and_a_rising_offset() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        watched(&mut config, &["imu", "nav"]);
        let mut coordinator = config.build(&table(&recorder)).unwrap();
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
        let mut coordinator = config.build(&table(&recorder)).unwrap();
        coordinator.run(utils::after_cycles(5)).await;
        assert_eq!(coordinator.cycle(), 5);
        assert_eq!(recorder.take().len(), 5);
    }

    #[stellarator::test]
    async fn an_overrun_cycle_yields_instead_of_sleeping() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        // A budget no cycle can meet, so every iteration takes the yield path.
        config.clock = Clock::Wall { rate: 1e12 };
        let mut coordinator = config.build(&table(&recorder)).unwrap();
        coordinator.run(utils::after_cycles(3)).await;
        assert_eq!(coordinator.cycle(), 3);
    }

    #[stellarator::test]
    async fn a_simulated_clock_advances_by_dt() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.clock = Clock::Simulated {
            dt: Duration::from_millis(20),
        };
        watched(&mut config, &["imu"]);
        let mut coordinator = config.build(&table(&recorder)).unwrap();
        let epoch = coordinator.epoch.0;
        coordinator.run(utils::after_cycles(3)).await;

        let stamps: Vec<_> = recorder
            .take_status()
            .into_iter()
            .map(|status| status.timestamp.0 - epoch)
            .collect();
        assert_eq!(stamps, vec![0, 20_000, 40_000]);
    }

    #[test]
    fn an_empty_coordinator_still_counts_cycles() {
        let mut coordinator = CoordinatorConfig::default()
            .build(&table(&Recorder::default()))
            .unwrap();
        coordinator.step(Timestamp(0));
        assert_eq!(coordinator.cycle(), 1);
    }

    /// `boom -> nav -> control`, with `boom`'s log and status drained after it.
    fn boom_config() -> CoordinatorConfig {
        let mut config = pipeline_config();
        config.systems[0] = SystemConfig::new("boom", "boom");
        config.systems[1].inputs[0].from = vec![PortRef::new("boom", "imu")];
        config.systems.push(SystemConfig {
            inputs: vec![InputConfig {
                port: "lines".into(),
                from: vec![PortRef::new("boom", "log")],
            }],
            ..SystemConfig::new("logs", "log_sink")
        });
        watched(&mut config, &["boom"]);
        config
    }

    #[test]
    fn a_panic_latches_one_system_and_leaves_the_cycle_running() {
        let recorder = Recorder::default();
        let mut coordinator = boom_config().build(&table(&recorder)).unwrap();
        coordinator.step(Timestamp(1));
        assert_eq!(coordinator.latched().count(), 0);
        assert!(recorder.take_logs().is_empty());

        coordinator.step(Timestamp(2));
        assert_eq!(coordinator.latched().collect::<Vec<_>>(), vec!["boom"]);
        // `nav` and `control` still ran on the cycle `boom` failed.
        assert_eq!(
            recorder.take(),
            vec![(Timestamp(1), 3.0), (Timestamp(1), 3.0)]
        );
    }

    #[test]
    fn a_panic_writes_one_fault_line_on_the_systems_log() {
        let recorder = Recorder::default();
        let mut coordinator = boom_config().build(&table(&recorder)).unwrap();
        coordinator.step(Timestamp(1));
        coordinator.step(Timestamp(2));
        coordinator.step(Timestamp(3));

        let lines = recorder.take_logs();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].level, metor_proto_wkt::LogLevel::Error);
        assert_eq!(lines[0].timestamp, Timestamp(2));
        assert_eq!(lines[0].fields, vec![("kind".into(), "panic".into())]);
        assert_eq!(lines[0].message, "boom on cycle 2");
    }

    #[test]
    fn a_latched_system_keeps_reporting_a_zero_execution_time() {
        let recorder = Recorder::default();
        let mut coordinator = boom_config().build(&table(&recorder)).unwrap();
        for cycle in 1..=3 {
            coordinator.step(Timestamp(cycle));
        }
        let seen = recorder.take_status();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[2].timestamp, Timestamp(3));
        assert_eq!(seen[2].exec_time_ns, 0);
    }

    #[test]
    fn a_trait_path_panic_latches_with_nothing_written() {
        let recorder = Recorder::default();
        let mut config = CoordinatorConfig {
            systems: vec![SystemConfig::new("trap", "trap")],
            ..Default::default()
        };
        watched(&mut config, &["trap"]);
        let mut coordinator = config.build(&table(&recorder)).unwrap();
        coordinator.step(Timestamp(1));
        coordinator.step(Timestamp(2));
        assert_eq!(coordinator.latched().collect::<Vec<_>>(), vec!["trap"]);
        assert!(recorder.take_logs().is_empty());
        assert_eq!(recorder.take_status().len(), 2);
    }

    #[test]
    fn a_payload_that_is_no_string_reads_as_panic() {
        struct Odd;
        impl Step for Odd {
            fn execute(&mut self, _now: Timestamp) {
                std::panic::panic_any(7u8);
            }
        }
        assert_eq!(message_of(&7u8), "panic");
        let mut step: Option<Box<dyn Step>> = Some(Box::new(Odd));
        assert!(!catch_step(&mut step, Timestamp(0)));
        assert!(step.is_none());
    }

    #[test]
    fn catch_step_passes_a_clean_step_through() {
        struct Clean(u64);
        impl Step for Clean {
            fn execute(&mut self, _now: Timestamp) {
                self.0 += 1;
            }
        }
        let mut step: Option<Box<dyn Step>> = Some(Box::new(Clean(0)));
        assert!(catch_step(&mut step, Timestamp(0)));
        assert!(step.is_some());
    }

    #[test]
    fn a_simulated_timestamp_saturates_instead_of_wrapping() {
        let far = simulated_time(Timestamp(i64::MAX), u64::MAX, Duration::from_secs(1));
        assert_eq!(far, Timestamp(i64::MAX));
        assert_eq!(
            simulated_time(Timestamp(5), 0, Duration::from_secs(1)),
            Timestamp(5)
        );
    }
}
