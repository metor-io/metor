//! The built [`Coordinator`] and the loop that runs it one cycle at a time.

use core::future::Future;
use core::pin::pin;
use std::time::{Duration, Instant};

use futures_lite::future::poll_once;
use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;

use crate::port::Output;

use super::config::Clock;
use super::status::SystemStatus;
use super::step::{EndReason, Step, Stepped, catch_step};

/// A system in the coordinator's schedule.
pub(super) struct Entry {
    pub(super) name: String,
    pub(super) step: Option<Box<dyn Step>>,
    pub(super) status: Output<SystemStatus>,
}

/// Runs systems in a fixed order, once per cycle.
///
/// Build one with [`CoordinatorConfig::build`](super::CoordinatorConfig::build).
/// Call [`step`](Coordinator::step) to run a single cycle at a time you choose,
/// or [`run`](Coordinator::run) to run cycles on the configured clock. A system
/// that panics is stopped, and the rest keep running.
pub struct Coordinator {
    pub(super) entries: Vec<Entry>,
    pub(super) clock: Clock,
    /// Time zero of the simulated clock, taken at build.
    pub(super) epoch: Timestamp,
    pub(super) cycle: u64,
    /// Keeps the ring memory alive alongside the systems' readers and writers.
    pub(super) rings: Vec<RingBuffer>,
}

impl Coordinator {
    /// Returns the number of rings allocated.
    ///
    /// There is one ring per output port, plus one status ring per system.
    pub fn ring_count(&self) -> usize {
        self.rings.len()
    }

    /// Returns each system that has ended, with its end reason, in step order.
    pub fn ended(&self) -> impl Iterator<Item = (&str, &EndReason)> {
        self.entries
            .iter()
            .filter_map(|entry| Some((entry.name.as_str(), entry.end_reason()?)))
    }

    /// Returns the ids of systems stopped by a panic, in step order.
    pub fn latched(&self) -> impl Iterator<Item = &str> {
        self.ended()
            .filter(|(_, end)| **end == EndReason::Panicked)
            .map(|(id, _)| id)
    }

    /// Returns the number of cycles completed so far.
    pub fn cycle(&self) -> u64 {
        self.cycle
    }

    /// Runs one cycle, executing each system once.
    ///
    /// A system that has ended, including one that panicked, is skipped. Its
    /// status is still written each cycle, with an execution time of zero.
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

    /// Runs cycles until `stop` completes.
    ///
    /// Under a wall clock, each cycle sleeps out the rest of its time budget.
    /// If a cycle overruns, or the clock is simulated, it yields instead.
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

    /// Returns the timestamp for the current cycle.
    fn now(&self) -> Timestamp {
        match self.clock {
            Clock::Wall { .. } => Timestamp::now(),
            Clock::Simulated { dt, .. } => simulated_time(self.epoch, self.cycle, dt),
        }
    }
}

impl Entry {
    /// Returns why this entry's step ended.
    ///
    /// A step that was dropped after a panic reports [`EndReason::Panicked`].
    pub(super) fn end_reason(&self) -> Option<&EndReason> {
        match &self.step {
            Some(step) => step.ended(),
            None => Some(&EndReason::Panicked),
        }
    }

    fn run(&mut self, now: Timestamp) -> Duration {
        if self.end_reason().is_some() {
            return Duration::ZERO;
        }
        let started = Instant::now();
        let stepped = catch_step(&mut self.step, now);
        let elapsed = started.elapsed();
        if stepped != Stepped::Ran && self.end_reason() == Some(&EndReason::Panicked) {
            crate::panic::catch(|| tracing::error!(system = self.name, "system panicked"));
        }
        elapsed
    }
}

/// Returns `epoch + cycle * dt`, saturating on overflow.
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
    use crate::coordinator::tests::Twice;
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
    fn test_pipeline_single_cycle() {
        let recorder = Recorder::default();
        let mut coordinator = pipeline_config().build(&table(&recorder)).unwrap();
        coordinator.step(Timestamp(10));
        assert_eq!(recorder.take(), vec![(Timestamp(1), 3.0)]);
        assert_eq!(coordinator.cycle(), 1);
    }

    #[test]
    fn test_consumer_reads_previous_cycle() {
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
    fn test_fan_in_selects_newest() {
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
    fn test_status_cycle_and_offset() {
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
    async fn test_run_stop() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.clock = Clock::Wall { rate: 10_000.0 };
        let mut coordinator = config.build(&table(&recorder)).unwrap();
        coordinator.run(utils::after_cycles(5)).await;
        assert_eq!(coordinator.cycle(), 5);
        assert_eq!(recorder.take().len(), 5);
    }

    #[stellarator::test]
    async fn test_overrun_yields() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        // A budget no cycle can meet, so every iteration takes the yield path.
        config.clock = Clock::Wall { rate: 1e12 };
        let mut coordinator = config.build(&table(&recorder)).unwrap();
        coordinator.run(utils::after_cycles(3)).await;
        assert_eq!(coordinator.cycle(), 3);
    }

    #[stellarator::test]
    async fn test_simulated_clock_step() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.clock = Clock::Simulated {
            dt: Duration::from_millis(20),
            epoch: None,
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

    #[stellarator::test]
    async fn test_epoch_fixes_the_first_cycle() {
        let recorder = Recorder::default();
        let mut config = pipeline_config();
        config.clock = Clock::Simulated {
            dt: Duration::from_millis(20),
            epoch: Some(Timestamp(1_000)),
        };
        watched(&mut config, &["imu"]);
        let mut coordinator = config.build(&table(&recorder)).unwrap();
        coordinator.run(utils::after_cycles(2)).await;
        let stamps: Vec<_> = recorder
            .take_status()
            .into_iter()
            .map(|status| status.timestamp.0)
            .collect();
        assert_eq!(stamps, vec![1_000, 21_000]);
    }

    #[test]
    fn test_clock_without_epoch_round_trips_unchanged() {
        let json = r#"{"Simulated":{"dt":{"secs":0,"nanos":10000000}}}"#;
        let clock: Clock = serde_json::from_str(json).unwrap();
        assert_eq!(
            clock,
            Clock::Simulated {
                dt: Duration::from_millis(10),
                epoch: None
            }
        );
        assert_eq!(serde_json::to_string(&clock).unwrap(), json);
    }

    #[test]
    fn test_empty_coordinator_cycles() {
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
            inputs: vec![InputConfig::new("lines", "boom", "log")],
            ..SystemConfig::new("logs", "log_sink")
        });
        watched(&mut config, &["boom"]);
        config
    }

    #[test]
    fn test_system_panic_isolation() {
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
    fn test_panic_fault_log() {
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
    fn test_latched_execution_time() {
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
    fn test_trait_panic_without_log() {
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
    fn test_non_string_panic_payload() {
        struct Odd;
        impl Step for Odd {
            fn execute(&mut self, _now: Timestamp) {
                std::panic::panic_any(7u8);
            }
        }
        assert_eq!(crate::panic::message(&7u8), "panic");
        let mut step: Option<Box<dyn Step>> = Some(Box::new(Odd));
        assert_eq!(catch_step(&mut step, Timestamp(0)), Stepped::Unwound);
        assert!(step.is_none());
        assert_eq!(catch_step(&mut step, Timestamp(1)), Stepped::Unwound);
    }

    #[test]
    fn test_catch_step_success() {
        struct Clean(u64);
        impl Step for Clean {
            fn execute(&mut self, _now: Timestamp) {
                self.0 += 1;
            }
        }
        let mut step: Option<Box<dyn Step>> = Some(Box::new(Clean(0)));
        assert_eq!(catch_step(&mut step, Timestamp(0)), Stepped::Ran);
        assert!(step.is_some());
    }

    #[test]
    fn test_catch_step_keeps_ended_step() {
        let runs = std::rc::Rc::default();
        let mut step: Option<Box<dyn Step>> = Some(Box::new(Twice(Clone::clone(&runs), None)));
        assert_eq!(catch_step(&mut step, Timestamp(0)), Stepped::Ran);
        assert_eq!(catch_step(&mut step, Timestamp(1)), Stepped::Ended);
        assert_eq!(catch_step(&mut step, Timestamp(2)), Stepped::Ended);
        assert_eq!(runs.get(), 2);
        assert!(step.is_some());
    }

    #[test]
    fn test_simulated_timestamp_saturation() {
        let far = simulated_time(Timestamp(i64::MAX), u64::MAX, Duration::from_secs(1));
        assert_eq!(far, Timestamp(i64::MAX));
        assert_eq!(
            simulated_time(Timestamp(5), 0, Duration::from_secs(1)),
            Timestamp(5)
        );
    }
}
