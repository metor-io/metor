use serde_json::json;

use super::*;
use super::{CONFIG_VERSION, TargetConfig};
use crate::coordinator::{CoordinatorConfig, InputConfig, OutputConfig};
use crate::link::{PUBLISH_TYPE, SUBSCRIBE_TYPE};
use crate::record::RecordSchema;
use crate::tests::utils::{Emit, Recorder, Tap, emit, table, tap};

fn tables(count: usize) -> Vec<SystemTable> {
    (0..count)
        .map(|_| {
            let recorder = Recorder::default();
            let mut table = table(&recorder);
            let register = || -> Result<(), crate::BuildError> {
                table.register(PUBLISH_TYPE, tap(), move || Tap(recorder.clone()))?;
                table.register(SUBSCRIBE_TYPE, emit(), || Emit(Vec::new()))
            };
            register().expect("valid records");
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

fn member(namespace: &str, systems: Vec<DeploySystem>) -> TargetConfig<DeploySystem> {
    let runtime = CoordinatorConfig {
        namespace: Some(namespace.into()),
        ..Default::default()
    };
    let json = serde_json::to_value(runtime).expect("encodes");
    TargetConfig {
        packs: Vec::new(),
        coordinator: CoordinatorConfig {
            systems,
            ..serde_json::from_value(json).expect("a runtime config is a deployment config")
        },
    }
}

/// Builds a two-target deployment. `a` publishes its `imu` frame and log
/// through `pub`. `b` subscribes to both and publishes on `out`. `a.cmds`
/// subscribes to `out` for log messages.
fn envelope() -> DeploymentConfig {
    let a = member(
        "a",
        vec![
            DeploySystem::new("imu", "imu"),
            DeploySystem::new("noisy", "boom"),
            DeploySystem {
                params: listen(2240),
                inputs: vec![
                    InputConfig::new("imu.imu", "imu", "imu"),
                    InputConfig::new("noisy.log", "noisy", "log"),
                ],
                ..DeploySystem::new("pub", PUBLISH_TYPE)
            },
            DeploySystem {
                params: peer("b", "out"),
                outputs: vec![OutputConfig {
                    port: "log".into(),
                    source: DeploySource::Runtime(OutputSource::Record("log".into())),
                }],
                ..DeploySystem::new("cmds", SUBSCRIBE_TYPE)
            },
        ],
    );
    let b = member(
        "b",
        vec![
            DeploySystem {
                params: peer("a", "pub"),
                outputs: vec![
                    OutputConfig {
                        port: "imu.imu".into(),
                        source: DeploySource::Peer(peer_port("a", "pub", "imu.imu")),
                    },
                    OutputConfig {
                        port: "log".into(),
                        source: DeploySource::Peer(peer_port("a", "pub", "noisy.log")),
                    },
                ],
                ..DeploySystem::new("mirror", SUBSCRIBE_TYPE)
            },
            DeploySystem {
                params: listen(2250),
                ..DeploySystem::new("out", PUBLISH_TYPE)
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
    let DeploySource::Runtime(OutputSource::Def(imu)) = &mirror.outputs[0].source else {
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
    let DeploySource::Runtime(OutputSource::Def(log)) = &mirror.outputs[1].source else {
        panic!("resolved to a def")
    };
    assert_eq!((log.name.as_ref(), log.record.as_ref()), ("log", "log"));
}

/// A peer output of a `status` edge takes the producer's status port.
#[test]
fn test_peer_output_of_a_status_edge() {
    let mut envelope = envelope();
    let status = InputConfig::new("imu.status", "imu", "status");
    envelope.targets[0].coordinator.systems[2]
        .inputs
        .push(status);
    envelope.targets[1].coordinator.systems[0]
        .outputs
        .push(OutputConfig {
            port: "imu.status".into(),
            source: DeploySource::Peer(peer_port("a", "pub", "imu.status")),
        });
    resolve(&mut envelope, &tables(2)).expect("resolves");
    let mirror = &envelope.targets[1].coordinator.systems[0];
    let DeploySource::Runtime(OutputSource::Def(status)) = &mirror.outputs[2].source else {
        panic!("resolved to a def")
    };
    let cx = crate::def::DefCx {
        id: "imu",
        namespace: Some("a"),
        ..crate::def::DefCx::empty()
    };
    let expected = crate::PortDef {
        name: "imu.status".into(),
        ..crate::SystemStatus::port_def(&cx)
    };
    assert_eq!(status, &expected);
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
fn test_resolved_envelope_is_a_fixpoint() {
    let mut envelope = envelope();
    assert!(unresolved(&envelope).expect("decodes"));
    resolve(&mut envelope, &tables(2)).expect("resolves");
    assert!(!unresolved(&envelope).expect("decodes"));
    let again = envelope.clone();
    resolve(&mut envelope, &tables(2)).expect("resolves again");
    assert_eq!(envelope, again);
    let runtime = envelope.clone().into_runtime().expect("resolved");
    let bytes = serde_json::to_vec(&runtime).expect("encodes");
    let reread = DeploymentConfig::from_slice(&bytes).expect("a runtime file is a deployment");
    assert_eq!(reread, envelope);
}

#[test]
fn test_deploy_source_forms() {
    let text = r#"{"port":"est.est","peer":{"target":"a","link":"pub","port":"est.est"}}"#;
    let peer: OutputConfig<DeploySource> = serde_json::from_str(text).expect("peer form");
    assert_eq!(
        peer.source,
        DeploySource::Peer(peer_port("a", "pub", "est.est"))
    );
    assert_eq!(serde_json::to_string(&peer).expect("encodes"), text);
    let record: OutputConfig<DeploySource> =
        serde_json::from_str(r#"{"port":"ping","record":"ping"}"#).expect("record form");
    let ping = DeploySource::Runtime(OutputSource::Record("ping".into()));
    assert_eq!(record.source, ping);
    assert!(serde_json::from_str::<OutputConfig<DeploySource>>(r#"{"port":"x"}"#).is_err());
}

#[test]
fn test_into_runtime_needs_resolved_outputs() {
    let Err(DeployError::Unresolved { system, port }) = envelope().into_runtime() else {
        panic!("a peer output has no runtime form")
    };
    assert_eq!((system.as_str(), port.as_str()), ("b.mirror", "imu.imu"));
    let runtime = resolved().into_runtime().expect("resolved");
    let mirror = &runtime.targets[1].coordinator.systems[0];
    assert!(matches!(mirror.outputs[0].source, OutputSource::Def(_)));
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
        DeploySource::Peer(peer_port("a", "pub", "imu.gyro"));
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
            second: "b.out".into(),
        })
    );
}

/// A deploy error about a system cites where the target file created it.
#[test]
fn test_errors_cite_the_system_location() {
    let at = |line| {
        Some(crate::coordinator::Location {
            file: "deploy.py".into(),
            line,
        })
    };
    let mut missing_link = envelope();
    let mirror = &mut missing_link.targets[1].coordinator.systems[0];
    mirror.params = peer("a", "nope");
    mirror.outputs.clear();
    mirror.location = at(8);
    let error = resolve(&mut missing_link, &tables(2)).expect_err("no such link");
    assert_eq!(
        error.to_string(),
        "deploy.py:8: `b.mirror` names link `a.nope`, which is not a system there"
    );

    let mut taken = envelope();
    taken.targets[1].coordinator.systems[1].params = listen(2240);
    taken.targets[1].coordinator.systems[1].location = at(12);
    let Err(DeployError::Located { location, source }) = resolve(&mut taken, &tables(2)) else {
        panic!("the second listener is located")
    };
    assert_eq!(location.line, 12);
    assert!(matches!(*source, DeployError::PortTaken { port: 2240, .. }));

    let mut unknown = envelope();
    unknown.targets[0].coordinator.systems[1].ty = "nope".into();
    unknown.targets[0].coordinator.systems[1].location = at(3);
    let error = resolve(&mut unknown, &tables(2)).expect_err("an unknown type");
    assert!(
        error.to_string().starts_with("target `a`: deploy.py:3: "),
        "{error}"
    );
}

#[test]
fn test_mirror_of_a_mirror_of_itself_cycles() {
    let mirror = |peer_target: &str| DeploySystem {
        params: peer(peer_target, "pub"),
        outputs: vec![OutputConfig {
            port: "mirror.imu".into(),
            source: DeploySource::Peer(peer_port(peer_target, "pub", "mirror.imu")),
        }],
        ..DeploySystem::new("mirror", SUBSCRIBE_TYPE)
    };
    let publish = DeploySystem {
        params: listen(1),
        inputs: vec![InputConfig::new("mirror.imu", "mirror", "mirror.imu")],
        ..DeploySystem::new("pub", PUBLISH_TYPE)
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

#[test]
fn test_non_link_transport_is_untouched() {
    let mut envelope = envelope();
    envelope.targets[0].coordinator.systems[0].params = peer("b", "out");
    let params = envelope.targets[0].coordinator.systems[0].params.clone();
    resolve(&mut envelope, &tables(2)).expect("resolves");
    assert!(!unresolved(&envelope).expect("decodes"));
    assert_eq!(envelope.targets[0].coordinator.systems[0].params, params);
}

#[test]
fn test_undecodable_link_names_itself() {
    let mut envelope = envelope();
    envelope.targets[0].coordinator.systems[2].params = json!({ "transport": 5 });
    let Err(DeployError::Params { system, .. }) = unresolved(&envelope) else {
        panic!("a link's params must decode")
    };
    assert_eq!(system, "pub");
    let Err(DeployError::Params { system, .. }) = resolve(&mut envelope, &tables(2)) else {
        panic!("a link's params must decode")
    };
    assert_eq!(system, "pub");
}

#[test]
fn test_resolve_keeps_other_params() {
    let mut envelope = envelope();
    envelope.targets[1].coordinator.systems[0].params["conn_cap"] = json!(64);
    envelope.targets[1].coordinator.systems[1].params["conn_cap"] = json!(32);
    resolve(&mut envelope, &tables(2)).expect("resolves");
    let [mirror, out] = &envelope.targets[1].coordinator.systems[..] else {
        panic!("two systems")
    };
    assert_eq!(mirror.params["conn_cap"], 64);
    assert_eq!(
        mirror.params["transport"]["connect"]["addr"],
        "127.0.0.1:2240"
    );
    assert_eq!(out.params["conn_cap"], 32);
    assert_eq!(out.params["transport"]["listen"]["max_connections"], 1);
}

#[test]
fn test_stored_slots_resolve_to_themselves() {
    let def = json!({ "name": "up", "inputs": [], "outputs": [] });
    let hash = blake3::hash(b"up").to_hex();
    let stored =
        json!({ "path": "/store/x/libup.so", "hash": hash.as_str(), "def": def, "kind": "dylib" });
    let slot = DeploySystem {
        params: json!({ "allow": [{ "name": "up", "ty": "up", "params": null,
            "stored": stored }], "initial": null }),
        ..DeploySystem::new("mode", crate::dl::SLOT_TYPE)
    };
    let mut envelope = envelope();
    envelope.targets[0].coordinator.systems.push(slot);
    let before = envelope.clone();
    resolve_slots(&mut envelope, &tables(2), std::path::Path::new("/no/store")).expect("resolves");
    assert_eq!(envelope, before);
    let mut stored = envelope.clone();
    resolve(&mut stored, &tables(2)).expect("resolves");
    assert!(!unresolved(&stored).expect("decodes"));
}
