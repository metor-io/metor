//! Tests the thread adapter on a live coordinator: delivery, drops, panics,
//! and placement.

use core::time::Duration;
use std::time::Instant;

use metor_proto::types::Timestamp;

use crate::coordinator::{BuildError, Coordinator, CoordinatorConfig, InputConfig, PortRef};
use crate::coordinator::{BuildOptions, Clock, SystemConfig, SystemTable};
use crate::tests::utils::{Recorder, ended_with, table};

/// Both ways a group runs: on its own thread, and turned by the coordinator.
const MODES: [bool; 2] = [false, true];

/// Builds `config`; turned, on a simulated clock from time zero.
fn build(
    mut config: CoordinatorConfig,
    table: &SystemTable,
    deterministic: bool,
) -> Result<Coordinator, BuildError> {
    if deterministic {
        config.clock = Clock::Simulated {
            dt: DT,
            epoch: Some(Timestamp(0)),
        };
    }
    config.build_with(table, BuildOptions { deterministic })
}

/// The simulated cycle time of a turned build.
const DT: Duration = Duration::from_millis(10);

/// Returns the config for a system on the named thread, or on the default
/// thread when `thread` is `None`.
fn placed(id: &str, ty: &str, thread: Option<&str>) -> SystemConfig {
    SystemConfig {
        adapter: thread.map(|thread| crate::Adapter::Thread(thread.to_string())),
        ..SystemConfig::new(id, ty)
    }
}

fn reading(id: &str, ty: &str, port: &str, from: PortRef) -> SystemConfig {
    SystemConfig {
        inputs: vec![InputConfig {
            port: port.into(),
            from: vec![from],
        }],
        ..SystemConfig::new(id, ty)
    }
}

/// Steps the coordinator until `done` returns true, pausing briefly each
/// cycle so the background thread can run.
fn step_until(coordinator: &mut Coordinator, mut done: impl FnMut() -> bool) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut cycle = 0;
    while !done() {
        assert!(Instant::now() < deadline, "the thread never delivered");
        cycle += 1;
        coordinator.step(Timestamp(cycle));
        std::thread::sleep(Duration::from_millis(1));
    }
    cycle as u64
}

#[test]
fn test_async_record_relay() {
    for deterministic in MODES {
        let recorder = Recorder::default();
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig::new("imu", "imu"),
                reading("relay", "relay", "imu", PortRef::new("imu", "imu")),
                reading("control", "control", "nav", PortRef::new("relay", "nav")),
            ],
            ..Default::default()
        };
        let mut coordinator =
            build(config, &table(&recorder), deterministic).expect("valid config");
        let mut seen = Vec::new();
        step_until(&mut coordinator, || {
            seen.extend(recorder.take());
            seen.len() >= 4
        });
        // The samples count up, and each command is its estimate plus one.
        let commands: Vec<f64> = seen.iter().map(|(_, command)| *command).collect();
        assert_eq!(&commands[..4], &[3.0, 5.0, 7.0, 9.0]);
    }
}

#[test]
fn test_mirror_overflow_report() {
    for deterministic in MODES {
        let recorder = Recorder::default();
        let config = CoordinatorConfig {
            ring_depth: core::num::NonZeroUsize::new(2).expect("nonzero"),
            systems: vec![
                SystemConfig::new("imu", "imu"),
                reading("sleeper", "sleeper", "imu", PortRef::new("imu", "imu")),
                reading("logs", "log_sink", "lines", PortRef::new("sleeper", "log")),
            ],
            ..Default::default()
        };
        let mut coordinator =
            build(config, &table(&recorder), deterministic).expect("valid config");
        let mut lines = Vec::new();
        step_until(&mut coordinator, || {
            lines.extend(recorder.take_logs());
            lines
                .iter()
                .any(|line| kind(line) == Some("mirror_dropped"))
        });
        let reports: Vec<_> = lines
            .iter()
            .filter(|line| kind(line) == Some("mirror_dropped"))
            .collect();
        assert!(reports[0].fields.iter().any(|(name, _)| name == "imu"));
        // No system is latched off: the producer's ring is fine, and only the
        // mirror drops records.
        assert!(ended_with(&coordinator, &crate::EndReason::Panicked).is_empty());
    }
}

/// Returns the `kind` field of a fault log line.
fn kind(line: &metor_proto_wkt::LogEvent) -> Option<&str> {
    line.fields
        .iter()
        .find(|(name, _)| name == "kind")
        .map(|(_, value)| value.as_ref())
}

#[test]
fn test_task_panic_isolation() {
    for deterministic in MODES {
        let recorder = Recorder::default();
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig::new("imu", "imu"),
                reading("boom", "async_boom", "imu", PortRef::new("imu", "imu")),
                reading("relay", "relay", "imu", PortRef::new("imu", "imu")),
                reading("control", "control", "nav", PortRef::new("relay", "nav")),
                reading("logs", "log_sink", "lines", PortRef::new("boom", "log")),
            ],
            ..Default::default()
        };
        let mut coordinator =
            build(config, &table(&recorder), deterministic).expect("valid config");
        let mut lines = Vec::new();
        step_until(&mut coordinator, || {
            lines.extend(recorder.take_logs());
            lines.iter().any(|line| kind(line) == Some("panic"))
        });
        assert_eq!(
            ended_with(&coordinator, &crate::EndReason::Panicked),
            vec!["boom"]
        );
        let panic = lines
            .iter()
            .find(|line| kind(line) == Some("panic"))
            .expect("a panic line");
        assert!(panic.message.contains("boom on the background thread"));
        // The sibling on the same thread keeps delivering.
        let mut seen = Vec::new();
        step_until(&mut coordinator, || {
            seen.extend(recorder.take());
            !seen.is_empty()
        });
    }
}

#[test]
fn test_constructor_panic_cleanup() {
    for deterministic in MODES {
        let recorder = Recorder::default();
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig::new("imu", "imu"),
                reading("relay", "relay", "imu", PortRef::new("imu", "imu")),
                reading("boom", "ctor_boom", "imu", PortRef::new("imu", "imu")),
            ],
            ..Default::default()
        };
        let started = Instant::now();
        let error = build(config, &table(&recorder), deterministic).err();
        let Some(BuildError::Params { id, source }) = error else {
            panic!("a panicking constructor is a param error")
        };
        assert_eq!(id, "boom");
        assert!(
            source.to_string().contains("a constructor that panics"),
            "{source}"
        );
        // If the panic had killed the thread, the drop would wait for the join
        // timeout.
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}

#[test]
fn test_coordinator_drop_joins_threads() {
    for deterministic in MODES {
        let recorder = Recorder::default();
        let config = CoordinatorConfig {
            systems: vec![
                SystemConfig::new("imu", "imu"),
                reading("sleeper", "sleeper", "imu", PortRef::new("imu", "imu")),
                placed("own", "who_am_i", Some("own")),
            ],
            ..Default::default()
        };
        let mut coordinator =
            build(config, &table(&recorder), deterministic).expect("valid config");
        coordinator.step(Timestamp(1));
        let started = Instant::now();
        drop(coordinator);
        // Detaching a thread would take the whole join timeout.
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}

#[test]
fn test_thread_placement() {
    let recorder = Recorder::default();
    let config = CoordinatorConfig {
        systems: vec![
            placed("a", "who_am_i", None),
            placed("b", "who_am_i", None),
            placed("c", "who_am_i", Some("own")),
            reading("watch_a", "control", "nav", PortRef::new("a", "nav")),
            reading("watch_b", "control", "nav", PortRef::new("b", "nav")),
            reading("watch_c", "control", "nav", PortRef::new("c", "nav")),
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table(&recorder)).expect("valid config");
    let mut ids = Vec::new();
    step_until(&mut coordinator, || {
        ids.extend(recorder.take().into_iter().map(|(_, command)| command));
        ids.len() >= 3
    });
    ids.sort_by(f64::total_cmp);
    ids.dedup();
    assert_eq!(ids.len(), 2, "two threads report two ids: {ids:?}");
}

#[test]
fn test_reject_cyclic_thread_placement() {
    let recorder = Recorder::default();
    let config = CoordinatorConfig {
        systems: vec![placed("imu", "imu", Some("io"))],
        ..Default::default()
    };
    assert_eq!(
        config.build(&table(&recorder)).err(),
        Some(BuildError::UnsupportedAdapter {
            id: "imu".into(),
            ty: "imu".into(),
            adapter: "on thread `io`".into(),
        })
    );
}

#[test]
fn test_corrupt_mirror_ends_the_system() {
    use metor_proto_wkt::LogEvent;

    use crate::coordinator::{EndReason, MakeCx, Params};
    use crate::mirror::tests::slot_cursor;
    use crate::tests::utils::{Imu, Nav, input, output, ring};

    let table = table(&Recorder::default());
    let entry = table.get("relay").expect("registered");
    let (imu, nav, log) = (ring::<Imu>(8, 1), ring::<Nav>(8, 1), ring::<LogEvent>(8, 1));
    let mut lines = input::<LogEvent>(&log);
    let mut threads = super::Threads::deterministic();
    let mut step = (entry.make)(MakeCx {
        id: "relay",
        adapter: None,
        def: &entry.def,
        params: Params(&serde_json::Value::Null),
        inputs: vec![vec![&imu]],
        outputs: vec![&nav, &log],
        threads: &mut threads,
        seed: 0,
        namespace: None,
        records: &Default::default(),
    })
    .expect("a placed relay");
    let (base, _) = nav.region();
    // SAFETY: the slot is inside the region, and nothing reads it concurrently.
    unsafe {
        base.add(slot_cursor(nav.config()))
            .cast::<u64>()
            .write(u64::MAX - 1)
    };
    output::<Imu>(&imu).write(&Imu::new(1, 1.0)).expect("room");

    let mut groups: Vec<_> = threads.into_turned().collect();
    for cycle in 1..=8 {
        let now = Timestamp(cycle * 1_000_000);
        groups.iter_mut().for_each(|group| group.turn(now));
        step.execute(now);
        if step.ended().is_some() {
            break;
        }
    }
    assert_eq!(
        step.ended(),
        Some(&EndReason::Failed("corrupt ring".into()))
    );
    let corrupt = lines
        .drain()
        .filter_map(Result::ok)
        .any(|line| kind(&line) == Some("corrupt"));
    assert!(corrupt, "a corrupt fault line");
}

mod turned {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::*;
    use crate::async_system::Stop;
    use crate::port::Output;
    use crate::tests::utils::Nav;

    /// Sleeps 50 ms of cycle time, then writes one estimate.
    #[crate::system]
    async fn napper(nav: &mut Output<Nav>, stop: Stop) {
        let _ = stop;
        stellarator::sleep(Duration::from_millis(50)).await;
        let _ = nav.write(&Nav {
            timestamp: Timestamp(0),
            estimate: 1.0,
        });
    }

    /// Counts its polls, yielding after each, forever.
    #[crate::system]
    async fn poller(#[state] polls: &mut Arc<AtomicU64>, stop: Stop) {
        while !stop.is_set() {
            polls.fetch_add(1, Ordering::Relaxed);
            stellarator::yield_now().await;
        }
    }

    /// Spawns a task that sets its flag.
    #[crate::system]
    async fn spawner(#[state] flag: &mut Arc<AtomicBool>, stop: Stop) {
        let flag = flag.clone();
        let _ = stellarator::spawn(async move { flag.store(true, Ordering::Relaxed) }).await;
        stop.wait().await;
    }

    #[test]
    fn constructor_spawns_onto_the_group() {
        let recorder = Recorder::default();
        let seen = Seen::default();
        let mut table = turned_table(&recorder, &seen);
        let flag = seen.flag.clone();
        let make = move || {
            let flag = flag.clone();
            drop(stellarator::spawn(async move {
                flag.store(true, Ordering::Relaxed)
            }));
        };
        table.register_async("eager", napper(), make).unwrap();
        let config = CoordinatorConfig {
            systems: vec![placed("e", "eager", None)],
            ..Default::default()
        };
        let mut c = build(config, &table, true).unwrap();
        cycles(&mut c, 0, 1);
        assert!(seen.flag.load(Ordering::Relaxed), "the group polled it");
    }

    /// Sets its flag when its future drops.
    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// Sleeps far past the run, holding a drop flag.
    #[crate::system]
    async fn long_nap(#[state] flag: &mut Arc<AtomicBool>, stop: Stop) {
        let _guard = Dropped(flag.clone());
        let _ = stop;
        stellarator::sleep(Duration::from_secs(3600)).await;
    }

    #[derive(Default)]
    struct Seen {
        polls: Arc<AtomicU64>,
        flag: Arc<AtomicBool>,
    }

    fn turned_table(recorder: &Recorder, seen: &Seen) -> SystemTable {
        let mut table = table(recorder);
        table
            .register_async("napper", napper(), Default::default)
            .unwrap();
        let polls = seen.polls.clone();
        table
            .register_async("poller", poller(), move || polls.clone())
            .unwrap();
        let flag = seen.flag.clone();
        table
            .register_async("spawner", spawner(), move || flag.clone())
            .unwrap();
        let flag = seen.flag.clone();
        table
            .register_async("long_nap", long_nap(), move || flag.clone())
            .unwrap();
        table
    }

    fn turned(systems: Vec<SystemConfig>, recorder: &Recorder, seen: &Seen) -> Coordinator {
        let config = CoordinatorConfig {
            systems,
            ..Default::default()
        };
        build(config, &turned_table(recorder, seen), true).unwrap()
    }

    /// Steps cycles `from..to` at the turned build's cycle times.
    fn cycles(c: &mut Coordinator, from: i64, to: i64) {
        let dt = DT.as_micros() as i64;
        (from..to).for_each(|k| c.step(Timestamp(k * dt)));
    }

    /// The group turns before the relay feeds its input, so each answer
    /// lands one cycle after its imu record.
    #[test]
    fn relay_answers_a_cycle_later_every_run() {
        let run = || {
            let recorder = Recorder::default();
            let systems = vec![
                SystemConfig::new("imu", "imu"),
                reading("relay", "relay", "imu", PortRef::new("imu", "imu")),
                reading("control", "control", "nav", PortRef::new("relay", "nav")),
            ];
            let mut c = turned(systems, &recorder, &Seen::default());
            cycles(&mut c, 0, 6);
            recorder.take()
        };
        let first = run();
        assert_eq!(
            first.len(),
            5,
            "an answer every cycle after the first: {first:?}"
        );
        assert_eq!(first, run());
    }

    #[test]
    fn systems_run_on_the_coordinator_thread() {
        let recorder = Recorder::default();
        let systems = vec![
            placed("a", "who_am_i", None),
            placed("b", "who_am_i", Some("own")),
            reading("watch_a", "control", "nav", PortRef::new("a", "nav")),
            reading("watch_b", "control", "nav", PortRef::new("b", "nav")),
        ];
        let mut c = turned(systems, &recorder, &Seen::default());
        cycles(&mut c, 0, 2);
        let ids: Vec<f64> = recorder.take().into_iter().map(|(_, id)| id).collect();
        // The control law adds one to each estimate.
        let here = crate::tests::utils::thread_id() as f64 + 1.0;
        assert!(
            !ids.is_empty() && ids.iter().all(|id| *id == here),
            "{ids:?}"
        );
    }

    #[test]
    fn sleep_wakes_on_the_cycle_its_deadline_passes() {
        for _ in 0..2 {
            let recorder = Recorder::default();
            let systems = vec![
                placed("nap", "napper", None),
                reading("watch", "control", "nav", PortRef::new("nap", "nav")),
            ];
            let mut c = turned(systems, &recorder, &Seen::default());
            cycles(&mut c, 0, 5);
            assert!(recorder.take().is_empty(), "asleep through cycle 4");
            cycles(&mut c, 5, 6);
            assert_eq!(recorder.take().len(), 1, "awake in cycle 5");
        }
    }

    /// A member that never stops is bounded by the scheduler's tick, and
    /// the group turns once a cycle however many members it has.
    #[test]
    fn group_turns_once_a_cycle() {
        let polls_with = |members: usize| {
            let seen = Seen::default();
            let systems = (0..members).map(|i| placed(&format!("p{i}"), "poller", None));
            let mut c = turned(systems.collect(), &Recorder::default(), &seen);
            cycles(&mut c, 0, 1);
            let before = seen.polls.load(Ordering::Relaxed);
            cycles(&mut c, 1, 11);
            seen.polls.load(Ordering::Relaxed) - before
        };
        let one = polls_with(1);
        assert!(one > 0);
        assert_eq!(polls_with(2), one);
    }

    #[test]
    fn spawn_inside_a_member_lands_on_the_group() {
        let seen = Seen::default();
        let flag = seen.flag.clone();
        let landed = stellarator::run(move || async move {
            let mut c = turned(
                vec![placed("s", "spawner", None)],
                &Recorder::default(),
                &seen,
            );
            for _ in 0..8 {
                stellarator::yield_now().await;
            }
            let before = flag.load(Ordering::Relaxed);
            cycles(&mut c, 0, 2);
            (before, flag.load(Ordering::Relaxed))
        });
        assert_eq!(landed, (false, true), "polled only during the step");
    }

    #[test]
    fn dropped_group_drops_a_sleeping_task() {
        let seen = Seen::default();
        let mut c = turned(
            vec![placed("n", "long_nap", None)],
            &Recorder::default(),
            &seen,
        );
        cycles(&mut c, 0, 3);
        assert!(!seen.flag.load(Ordering::Relaxed));
        drop(c);
        assert!(seen.flag.load(Ordering::Relaxed));
    }

    #[test]
    fn deterministic_needs_a_simulated_nonzero_clock() {
        let table = table(&Recorder::default());
        let options = BuildOptions {
            deterministic: true,
        };
        let wall = CoordinatorConfig::default().build_with(&table, options);
        assert_eq!(wall.err(), Some(BuildError::DeterministicWall));
        let zero = CoordinatorConfig {
            clock: Clock::Simulated {
                dt: Duration::ZERO,
                epoch: None,
            },
            ..Default::default()
        };
        let zero = zero.build_with(&table, options);
        assert_eq!(zero.err(), Some(BuildError::DeterministicZeroDt));
        let clock = |dt, epoch| CoordinatorConfig {
            clock: Clock::Simulated { dt, epoch },
            ..Default::default()
        };
        let sub_micro = clock(Duration::from_nanos(500), Some(Timestamp(0)));
        let sub_micro = sub_micro.build_with(&table, options);
        assert_eq!(sub_micro.err(), Some(BuildError::DeterministicZeroDt));
        for epoch in [None, Some(Timestamp(-1))] {
            let unfixed = clock(DT, epoch).build_with(&table, options);
            assert_eq!(unfixed.err(), Some(BuildError::DeterministicEpoch));
        }
    }
}
