//! The adapter on a live coordinator: delivery, drops, panics, and placement.

use core::time::Duration;
use std::time::Instant;

use metor_proto::types::Timestamp;

use crate::coordinator::{BuildError, Coordinator, CoordinatorConfig, InputConfig, PortRef};
use crate::coordinator::{ParamError, SystemConfig};
use crate::tests::utils::{Recorder, table};

#[test]
fn a_mirror_copies_only_records_committed_before_the_drain() {
    use core::cell::RefCell;
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer, WakeSource, Writer};

    struct Refill(RefCell<Writer<NoWake>>);
    impl WakeSource for Refill {
        fn notify(&self) {
            self.0
                .borrow_mut()
                .try_write(b"later")
                .expect("source has room");
        }
    }

    let source = RingBuffer::create_in_memory(Config {
        capacity: 256,
        max_readers: 1,
    });
    let target = RingBuffer::create_in_memory(Config {
        capacity: 256,
        max_readers: 1,
    });
    let mut writer = source.writer(NoWake).expect("writer");
    let mut received = target.view(NoWake).expect("reader");
    let mut mirror = super::Mirror {
        port: "out".into(),
        from: source.view(NoWake).expect("reader"),
        into: {
            writer.try_write(b"first").expect("room");
            writer.try_write(b"second").expect("room");
            target.writer(Refill(RefCell::new(writer))).expect("writer")
        },
        dropped: 0,
    };
    mirror.drain();
    assert_eq!(
        received
            .drain()
            .collect::<Result<Vec<_>, _>>()
            .expect("valid"),
        [b"first".as_slice(), b"second".as_slice()]
    );
    assert_eq!(
        mirror
            .from
            .drain()
            .collect::<Result<Vec<_>, _>>()
            .expect("valid"),
        [b"later".as_slice(), b"later".as_slice()]
    );
    assert_eq!(mirror.dropped, 0);
}

/// A system on a named thread, or the shared one when `thread` is `None`.
fn placed(id: &str, ty: &str, thread: Option<&str>) -> SystemConfig {
    SystemConfig {
        thread: thread.map(str::to_string),
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

/// Steps until `done`, giving the background thread a moment each cycle.
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
fn an_async_system_relays_every_record_into_the_graph() {
    let recorder = Recorder::default();
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("imu", "imu"),
            reading("relay", "relay", "imu", PortRef::new("imu", "imu")),
            reading("control", "control", "nav", PortRef::new("relay", "nav")),
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table(&recorder)).expect("valid config");
    let mut seen = Vec::new();
    step_until(&mut coordinator, || {
        seen.extend(recorder.take());
        seen.len() >= 4
    });
    // Each command is one estimate plus one, over samples counting up.
    let commands: Vec<f64> = seen.iter().map(|(_, command)| *command).collect();
    assert_eq!(&commands[..4], &[3.0, 5.0, 7.0, 9.0]);
}

#[test]
fn a_system_that_never_reads_drops_into_its_mirror_and_reports_it() {
    let recorder = Recorder::default();
    let config = CoordinatorConfig {
        ring_depth: 2,
        systems: vec![
            SystemConfig::new("imu", "imu"),
            reading("sleeper", "sleeper", "imu", PortRef::new("imu", "imu")),
            reading("logs", "log_sink", "lines", PortRef::new("sleeper", "log")),
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table(&recorder)).expect("valid config");
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
    // The producer keeps its own ring; only the mirror refuses records.
    assert!(coordinator.latched().next().is_none());
}

#[test]
fn a_mirror_too_large_for_a_region_is_a_param_error() {
    assert!(super::mirror_config(64).is_ok());
    // Four times a capacity no region can hold, and one that overflows.
    assert!(matches!(
        super::mirror_config(3),
        Err(ParamError::Decode(_))
    ));
    assert!(matches!(
        super::mirror_config(usize::MAX / 2),
        Err(ParamError::Decode(_))
    ));
}

/// The `kind` field a fault line carries.
fn kind(line: &metor_proto_wkt::LogEvent) -> Option<&str> {
    line.fields
        .iter()
        .find(|(name, _)| name == "kind")
        .map(|(_, value)| value.as_ref())
}

#[test]
fn a_panicked_task_latches_its_system_and_leaves_the_thread_running() {
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
    let mut coordinator = config.build(&table(&recorder)).expect("valid config");
    let mut lines = Vec::new();
    step_until(&mut coordinator, || {
        lines.extend(recorder.take_logs());
        lines.iter().any(|line| kind(line) == Some("panic"))
    });
    assert_eq!(coordinator.latched().collect::<Vec<_>>(), vec!["boom"]);
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

#[test]
fn a_constructor_that_panics_names_its_system_and_frees_its_thread() {
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
    let error = config.build(&table(&recorder)).err();
    let Some(BuildError::Params { id, source }) = error else {
        panic!("a panicking constructor is a param error")
    };
    assert_eq!(id, "boom");
    assert!(
        source.to_string().contains("a constructor that panics"),
        "{source}"
    );
    // A thread taken down by the panic would be detached at the join timeout.
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn dropping_the_coordinator_joins_every_thread() {
    let recorder = Recorder::default();
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("imu", "imu"),
            reading("sleeper", "sleeper", "imu", PortRef::new("imu", "imu")),
            placed("own", "who_am_i", Some("own")),
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table(&recorder)).expect("valid config");
    coordinator.step(Timestamp(1));
    let started = Instant::now();
    drop(coordinator);
    // A detached thread would take the whole join timeout.
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn placement_shares_one_thread_and_honors_a_named_one() {
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
fn a_cyclic_system_cannot_be_placed_on_a_thread() {
    let recorder = Recorder::default();
    let config = CoordinatorConfig {
        systems: vec![placed("imu", "imu", Some("io"))],
        ..Default::default()
    };
    assert_eq!(
        config.build(&table(&recorder)).err(),
        Some(BuildError::ThreadOnCyclic {
            id: "imu".into(),
            thread: "io".into(),
        })
    );
}
