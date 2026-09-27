use metor_proto_wkt::LogEvent;
use serde_json::json;

use super::*;
use crate::cli::config::{CONFIG_VERSION, TargetConfig};
use crate::coordinator::{CoordinatorConfig, InputConfig, OutputConfig, PeerPort, SystemConfig};
use crate::record::Record;
use crate::tests::utils::{Recorder, imu_source, table};

fn tables(count: usize) -> Vec<SystemTable> {
    (0..count)
        .map(|_| {
            let mut table = table(&Recorder::default());
            table
                .register(DB_TYPE, imu_source(), i64::default)
                .expect("valid records");
            table
        })
        .collect()
}

fn listen(port: u16) -> serde_json::Value {
    json!({ "transport": { "listen": { "addr": format!("127.0.0.1:{port}"), "max_connections": 1 } } })
}

fn peer(target: &str, link: &str) -> serde_json::Value {
    json!({ "transport": { "peer": { "target": target, "link": link } } })
}

fn peer_port(target: &str, link: &str, port: &str) -> PeerPort {
    PeerPort {
        target: target.into(),
        link: link.into(),
        port: port.into(),
    }
}

fn member(namespace: &str, systems: Vec<SystemConfig>) -> TargetConfig {
    TargetConfig {
        packs: Vec::new(),
        coordinator: CoordinatorConfig {
            namespace: Some(namespace.into()),
            systems,
            ..Default::default()
        },
    }
}

/// `a` publishes its `imu` frame and log through `pub`; `b` mirrors both and
/// runs a db; `a.cmds` subscribes to that db for log messages.
fn envelope() -> DeploymentConfig {
    let a = member(
        "a",
        vec![
            SystemConfig::new("imu", "imu"),
            SystemConfig::new("noisy", "boom"),
            SystemConfig {
                params: listen(2240),
                inputs: vec![
                    InputConfig::new("imu.imu", "imu", "imu"),
                    InputConfig::new("noisy.log", "noisy", "log"),
                ],
                ..SystemConfig::new("pub", "tap")
            },
            SystemConfig {
                params: peer("b", "db"),
                outputs: vec![OutputConfig::record("log", "log")],
                ..SystemConfig::new("cmds", "emit")
            },
        ],
    );
    let b = member(
        "b",
        vec![
            SystemConfig {
                params: peer("a", "pub"),
                outputs: vec![
                    OutputConfig {
                        port: "imu.imu".into(),
                        source: OutputSource::Peer(peer_port("a", "pub", "imu.imu")),
                    },
                    OutputConfig {
                        port: "log".into(),
                        source: OutputSource::Peer(peer_port("a", "pub", "noisy.log")),
                    },
                ],
                ..SystemConfig::new("mirror", "emit")
            },
            SystemConfig {
                params: listen(2250),
                ..SystemConfig::new("db", DB_TYPE)
            },
        ],
    );
    DeploymentConfig {
        config_version: CONFIG_VERSION,
        hosts: Default::default(),
        targets: vec![a, b],
    }
}

fn resolved() -> DeploymentConfig {
    let mut envelope = envelope();
    resolve(&mut envelope, &tables(2)).expect("resolves");
    envelope
}

#[test]
fn test_peer_output_takes_producer_def() {
    let envelope = resolved();
    let mirror = &envelope.targets[1].coordinator.systems[0];
    let OutputSource::Def(imu) = &mirror.outputs[0].source else {
        panic!("resolved to a def")
    };
    assert_eq!(imu.name, "imu.imu");
    assert_eq!(imu.record, "imu");
    let RecordSchema::Frame { metadata, .. } = &imu.schema else {
        panic!("a frame")
    };
    assert_eq!(
        metadata[0].name, "a.imu.imu.sample",
        "the peer's root is kept"
    );
    let OutputSource::Def(log) = &mirror.outputs[1].source else {
        panic!("resolved to a def")
    };
    assert_eq!((log.name.as_ref(), log.record.as_ref()), ("log", "log"));
}

#[test]
fn test_peer_transport_takes_host_address() {
    let envelope = resolved();
    let mirror = &envelope.targets[1].coordinator.systems[0];
    assert_eq!(
        mirror.params["transport"],
        json!({ "connect": { "addr": "127.0.0.1:2240" } })
    );
    let mut placed = self::envelope();
    placed.hosts.insert("a".into(), "10.0.0.2".into());
    placed.hosts.insert("b".into(), "::1".into());
    resolve(&mut placed, &tables(2)).expect("resolves");
    let mirror = &placed.targets[1].coordinator.systems[0];
    assert_eq!(
        mirror.params["transport"]["connect"]["addr"],
        "10.0.0.2:2240"
    );
    let cmds = &placed.targets[0].coordinator.systems[3];
    assert_eq!(cmds.params["transport"]["connect"]["addr"], "[::1]:2250");
}

#[test]
fn test_db_collects_subscriber_command_ids() {
    let envelope = resolved();
    let db = &envelope.targets[1].coordinator.systems[1];
    let id = LogEvent::schema().packet_id();
    assert_eq!(db.params["command_ids"], json!([id]));
}

#[test]
fn test_resolved_envelope_is_a_fixpoint() {
    let mut envelope = envelope();
    assert!(unresolved(&envelope));
    resolve(&mut envelope, &tables(2)).expect("resolves");
    assert!(!unresolved(&envelope));
    let again = envelope.clone();
    resolve(&mut envelope, &tables(2)).expect("resolves again");
    assert_eq!(envelope, again);
}

#[test]
fn test_missing_references_name_themselves() {
    let mut missing_target = envelope();
    missing_target.targets[1].coordinator.systems[0].params = peer("c", "pub");
    missing_target.targets[1].coordinator.systems[0]
        .outputs
        .clear();
    assert_eq!(
        resolve(&mut missing_target, &tables(2)),
        Err(DeployError::NoSuchTarget {
            from: "b.mirror".into(),
            target: "c".into(),
        })
    );

    let mut missing_link = envelope();
    missing_link.targets[1].coordinator.systems[0].params = peer("a", "nope");
    missing_link.targets[1].coordinator.systems[0]
        .outputs
        .clear();
    assert_eq!(
        resolve(&mut missing_link, &tables(2)),
        Err(DeployError::NoSuchLink {
            from: "b.mirror".into(),
            target: "a".into(),
            link: "nope".into(),
        })
    );

    let mut missing_port = envelope();
    missing_port.targets[1].coordinator.systems[0].outputs[0].source =
        OutputSource::Peer(peer_port("a", "pub", "imu.gyro"));
    assert_eq!(
        resolve(&mut missing_port, &tables(2)),
        Err(DeployError::NoSuchPort {
            from: "b.mirror".into(),
            target: "a".into(),
            link: "pub".into(),
            port: "imu.gyro".into(),
        })
    );
}

#[test]
fn test_peer_must_listen_on_a_fixed_port() {
    let not_a_listener = DeployError::NotAListener {
        from: "b.mirror".into(),
        target: "a".into(),
        link: "pub".into(),
    };
    let mut dialing = envelope();
    dialing.targets[0].coordinator.systems[2].params =
        json!({ "transport": { "connect": { "addr": "127.0.0.1:1" } } });
    assert_eq!(
        resolve(&mut dialing, &tables(2)),
        Err(not_a_listener.clone())
    );

    let mut any_port = envelope();
    any_port.targets[0].coordinator.systems[2].params = listen(0);
    assert_eq!(resolve(&mut any_port, &tables(2)), Err(not_a_listener));
}

#[test]
fn test_two_listeners_on_one_host_port() {
    let mut envelope = envelope();
    envelope.targets[1].coordinator.systems[1].params = listen(2240);
    assert_eq!(
        resolve(&mut envelope, &tables(2)),
        Err(DeployError::PortTaken {
            host: LOOPBACK.into(),
            port: 2240,
            first: "a.pub".into(),
            second: "b.db".into(),
        })
    );
}

#[test]
fn test_mirror_of_a_mirror_of_itself_cycles() {
    let mirror = |peer_target: &str| SystemConfig {
        params: peer(peer_target, "pub"),
        outputs: vec![OutputConfig {
            port: "mirror.imu".into(),
            source: OutputSource::Peer(peer_port(peer_target, "pub", "mirror.imu")),
        }],
        ..SystemConfig::new("mirror", "emit")
    };
    let publish = SystemConfig {
        params: listen(1),
        inputs: vec![InputConfig::new("mirror.imu", "mirror", "mirror.imu")],
        ..SystemConfig::new("pub", "tap")
    };
    let mut envelope = DeploymentConfig {
        config_version: CONFIG_VERSION,
        hosts: Default::default(),
        targets: vec![
            member("a", vec![mirror("b"), publish.clone()]),
            member("b", vec![mirror("a"), publish]),
        ],
    };
    envelope.targets[1].coordinator.systems[1].params = listen(2);
    let Err(DeployError::Cycle { path }) = resolve(&mut envelope, &tables(2)) else {
        panic!("a mirror of a mirror of itself cycles")
    };
    let names: Vec<&str> = path.split(" -> ").collect();
    assert_eq!(names.first(), names.last(), "{path}");
    for name in ["a.mirror", "a.pub", "b.mirror", "b.pub"] {
        assert!(names.contains(&name), "{path}");
    }
}

#[test]
fn test_unknown_type_is_a_build_error() {
    let mut envelope = envelope();
    envelope.targets[0].coordinator.systems[1].ty = "nope".into();
    assert!(matches!(
        resolve(&mut envelope, &tables(2)),
        Err(DeployError::Build { target, source: crate::BuildError::UnknownType { .. } }) if target == "a"
    ));
}
