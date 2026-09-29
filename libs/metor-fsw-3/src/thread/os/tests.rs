//! Tests the thread adapter on a live coordinator: delivery, drops, panics,
//! and placement.

use core::time::Duration;
use std::time::Instant;

use metor_proto::types::Timestamp;

use crate::coordinator::{BuildError, Coordinator, CoordinatorConfig, InputConfig, PortRef};
use crate::coordinator::{ParamError, SystemConfig};
use crate::tests::utils::{Recorder, table};

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
    // The samples count up, and each command is its estimate plus one.
    let commands: Vec<f64> = seen.iter().map(|(_, command)| *command).collect();
    assert_eq!(&commands[..4], &[3.0, 5.0, 7.0, 9.0]);
}

#[test]
fn test_mirror_overflow_report() {
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
    // No system is latched off: the producer's ring is fine, and only the
    // mirror drops records.
    assert!(coordinator.latched().next().is_none());
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
fn test_constructor_panic_cleanup() {
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
    // If the panic had killed the thread, the drop would wait for the join
    // timeout.
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn test_coordinator_drop_joins_threads() {
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
    // Detaching a thread would take the whole join timeout.
    assert!(started.elapsed() < Duration::from_millis(500));
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
        Some(BuildError::Params {
            id: "imu".into(),
            source: ParamError::Thread {
                thread: "io".into()
            },
        })
    );
}

fn channel(id: &str, ty: &str, name: &str) -> SystemConfig {
    SystemConfig {
        adapter: Some(crate::Adapter::Channel(crate::ChannelConfig {
            name: name.into(),
            start: crate::RunState::Running,
        })),
        ..SystemConfig::new(id, ty)
    }
}

#[test]
fn test_reject_channel_on_async() {
    let config = CoordinatorConfig {
        systems: vec![channel("sleeper", "sleeper", "mode")],
        ..Default::default()
    };
    assert_eq!(
        config.build(&table(&Recorder::default())).err(),
        Some(BuildError::Params {
            id: "sleeper".into(),
            source: ParamError::Channel {
                channel: "mode".into()
            },
        })
    );
}

#[test]
fn test_reject_bad_channel_names() {
    let long = "c".repeat(metor_proto_wkt::SEQUENCE_CHANNEL_NAME_CAP + 1);
    let config = CoordinatorConfig {
        systems: vec![channel("imu", "imu", &long)],
        ..Default::default()
    };
    assert!(matches!(
        config.build(&table(&Recorder::default())).err(),
        Some(BuildError::ChannelName { .. })
    ));
    let config = CoordinatorConfig {
        systems: vec![channel("a", "imu", "mode"), channel("b", "imu", "mode")],
        ..Default::default()
    };
    assert_eq!(
        config.build(&table(&Recorder::default())).err(),
        Some(BuildError::DuplicateChannel {
            name: "mode".into()
        })
    );
}

#[test]
fn test_cyclic_channel_at_cap_builds() {
    let name = "c".repeat(metor_proto_wkt::SEQUENCE_CHANNEL_NAME_CAP);
    let config = CoordinatorConfig {
        systems: vec![channel("imu", "imu", &name)],
        ..Default::default()
    };
    assert!(config.build(&table(&Recorder::default())).is_ok());
}
