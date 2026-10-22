use serde_json::json;

use super::*;
use super::{CONFIG_VERSION, TargetConfig};
use crate::coordinator::{CoordinatorConfig, InputConfig, OutputConfig};
use crate::link::{DB_TYPE, PUBLISH_TYPE, SUBSCRIBE_TYPE};
use crate::record::RecordSchema;
use crate::tests::utils::{Emit, Recorder, Tap, emit, imu_source, table, tap};

/// Resolves whatever `envelope` has pending, with no store.
fn resolve(envelope: &mut DeploymentConfig, tables: &[SystemTable]) -> Result<(), DeployError> {
    match pending(envelope)? {
        Some(pending) => pending.resolve(envelope, tables, std::path::Path::new("/no/store")),
        None => Ok(()),
    }
}

fn unresolved(envelope: &DeploymentConfig) -> Result<bool, DeployError> {
    Ok(pending(envelope)?.is_some())
}

fn tables(count: usize) -> Vec<SystemTable> {
    (0..count)
        .map(|_| {
            let recorder = Recorder::default();
            let mut table = table(&recorder);
            let register = || -> Result<(), crate::BuildError> {
                table.register(PUBLISH_TYPE, tap(), move || Tap(recorder.clone()))?;
                table.register(SUBSCRIBE_TYPE, emit(), || Emit(Vec::new()))?;
                table.register(DB_TYPE, imu_source(), i64::default)
            };
            register().expect("valid records");
            table
        })
        .collect()
}

/// A resource that listens on `port`.
fn listen(port: u16) -> DeployBacking {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let allow = Vec::new();
    DeployBacking::Runtime(Backing::Real(RealKind::TcpListen { addr, allow }))
}

/// A resource that dials `addr`.
fn dial(addr: &str) -> DeployBacking {
    let addr = addr.parse().expect("an address");
    DeployBacking::Runtime(Backing::Real(RealKind::TcpDial { addr }))
}

/// A db's params, listening on `port`.
fn db_listen(port: u16) -> serde_json::Value {
    json!({ "listen": format!("127.0.0.1:{port}") })
}

fn peer(target: &str, link: &str) -> DeployBacking {
    DeployBacking::Peer(PeerLink {
        target: target.into(),
        link: link.into(),
    })
}

fn peer_port(target: &str, link: &str, port: &str) -> PeerPort {
    PeerPort {
        target: target.into(),
        link: link.into(),
        port: port.into(),
    }
}

/// A link `id` of type `ty`, granted resource `{id}_ep` as its `ep`.
fn link(id: &str, ty: &str) -> DeploySystem {
    DeploySystem {
        grants: [("ep".to_string(), format!("{id}_ep"))].into(),
        ..DeploySystem::new(id, ty)
    }
}

fn member(
    namespace: &str,
    systems: Vec<DeploySystem>,
    resources: Vec<(&str, DeployBacking)>,
) -> TargetConfig<DeploySystem, DeployBacking> {
    let runtime = CoordinatorConfig {
        namespace: Some(namespace.into()),
        ..Default::default()
    };
    let json = serde_json::to_value(runtime).expect("encodes");
    let resources = resources.into_iter().map(|(id, r)| (id.to_string(), r));
    TargetConfig {
        packs: Vec::new(),
        coordinator: CoordinatorConfig {
            systems,
            resources: resources.collect(),
            ..serde_json::from_value(json).expect("a runtime config is a deployment config")
        },
    }
}

/// The resource `id` of target `target`.
fn resource<'a>(envelope: &'a DeploymentConfig, target: usize, id: &str) -> &'a DeployBacking {
    &envelope.targets[target].coordinator.resources[id]
}

fn set_resource(envelope: &mut DeploymentConfig, target: usize, id: &str, to: DeployBacking) {
    envelope.targets[target]
        .coordinator
        .resources
        .insert(id.into(), to);
}

/// Builds a two-target deployment. `a` publishes its `imu` frame and log
/// through `pub`. `b` subscribes to both and runs a db. `a.cmds` subscribes
/// to that db for log messages.
fn envelope() -> DeploymentConfig {
    let a = member(
        "a",
        vec![
            DeploySystem::new("imu", "imu"),
            DeploySystem::new("noisy", "boom"),
            DeploySystem {
                inputs: vec![
                    InputConfig::new("imu.imu", "imu", "imu"),
                    InputConfig::new("noisy.log", "noisy", "log"),
                ],
                ..link("pub", PUBLISH_TYPE)
            },
            DeploySystem {
                outputs: vec![OutputConfig {
                    port: "log".into(),
                    source: DeploySource::Runtime(OutputSource::Record("log".into())),
                }],
                ..link("cmds", SUBSCRIBE_TYPE)
            },
        ],
        vec![("pub_ep", listen(2240)), ("cmds_ep", peer("b", "db"))],
    );
    let b = member(
        "b",
        vec![
            DeploySystem {
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
                ..link("mirror", SUBSCRIBE_TYPE)
            },
            DeploySystem {
                params: db_listen(2250),
                ..DeploySystem::new("db", DB_TYPE)
            },
        ],
        vec![("mirror_ep", peer("a", "pub"))],
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
fn test_peer_dial_takes_host_address() {
    let envelope = resolved();
    assert_eq!(resource(&envelope, 1, "mirror_ep"), &dial("127.0.0.1:2240"));
    let mut placed = self::envelope();
    placed.hosts.insert("a".into(), "10.0.0.2".into());
    placed.hosts.insert("b".into(), "::1".into());
    resolve(&mut placed, &tables(2)).expect("resolves");
    assert_eq!(resource(&placed, 1, "mirror_ep"), &dial("10.0.0.2:2240"));
    assert_eq!(resource(&placed, 0, "cmds_ep"), &dial("[::1]:2250"));
}

#[test]
fn test_peer_dial_resolves_a_host_name() {
    let mut named = envelope();
    named.hosts.insert("a".into(), "localhost".into());
    resolve(&mut named, &tables(2)).expect("resolves");
    let DeployBacking::Runtime(Backing::Real(RealKind::TcpDial { addr })) =
        resource(&named, 1, "mirror_ep")
    else {
        panic!("a resolved dial")
    };
    assert_eq!(
        addr,
        &SocketAddr::from(([127, 0, 0, 1], 2240)),
        "IPv4 first"
    );

    let mut unknown = envelope();
    unknown.hosts.insert("a".into(), "nohost.invalid".into());
    let error = resolve(&mut unknown, &tables(2)).expect_err("no such host");
    assert!(
        matches!(&error, DeployError::Address { from, .. } if from == "b.mirror"),
        "{error}"
    );
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
fn test_into_runtime_needs_resolved_dials() {
    let mut envelope = envelope();
    envelope.targets[1].coordinator.systems[0].outputs.clear();
    let Err(DeployError::UnresolvedDial { resource }) = envelope.into_runtime() else {
        panic!("a peer dial has no runtime form")
    };
    assert_eq!(resource, "a.cmds_ep");
}

/// A peer dial no system holds is left as it is, and stays unresolved.
#[test]
fn test_ungranted_peer_dial_stays_unresolved() {
    let mut envelope = envelope();
    set_resource(&mut envelope, 1, "spare", peer("a", "pub"));
    resolve(&mut envelope, &tables(2)).expect("resolves");
    assert_eq!(resource(&envelope, 1, "spare"), &peer("a", "pub"));
    assert!(matches!(
        envelope.into_runtime(),
        Err(DeployError::UnresolvedDial { resource }) if resource == "b.spare"
    ));
}

#[test]
fn test_into_runtime_needs_resolved_outputs() {
    let mut envelope = envelope();
    set_resource(&mut envelope, 0, "cmds_ep", dial("127.0.0.1:2250"));
    let Err(DeployError::Unresolved { system, port }) = envelope.into_runtime() else {
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
    set_resource(&mut missing_target, 1, "mirror_ep", peer("c", "pub"));
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
    set_resource(&mut missing_link, 1, "mirror_ep", peer("a", "nope"));
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
    set_resource(&mut dialing, 0, "pub_ep", dial("127.0.0.1:1"));
    assert_eq!(
        resolve(&mut dialing, &tables(2)),
        Err(not_a_listener.clone())
    );

    let mut any_port = envelope();
    set_resource(&mut any_port, 0, "pub_ep", listen(0));
    assert_eq!(resolve(&mut any_port, &tables(2)), Err(not_a_listener));
}

#[test]
fn test_two_listeners_on_one_host_port() {
    let mut envelope = envelope();
    envelope.targets[1].coordinator.systems[1].params = db_listen(2240);
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
    set_resource(&mut missing_link, 1, "mirror_ep", peer("a", "nope"));
    let mirror = &mut missing_link.targets[1].coordinator.systems[0];
    mirror.outputs.clear();
    mirror.location = at(8);
    let error = resolve(&mut missing_link, &tables(2)).expect_err("no such link");
    assert_eq!(
        error.to_string(),
        "deploy.py:8: `b.mirror` names link `a.nope`, which is not a system there"
    );

    let mut taken = envelope();
    taken.targets[1].coordinator.systems[1].params = db_listen(2240);
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
        outputs: vec![OutputConfig {
            port: "mirror.imu".into(),
            source: DeploySource::Peer(peer_port(peer_target, "pub", "mirror.imu")),
        }],
        ..link("mirror", SUBSCRIBE_TYPE)
    };
    let publish = DeploySystem {
        inputs: vec![InputConfig::new("mirror.imu", "mirror", "mirror.imu")],
        ..link("pub", PUBLISH_TYPE)
    };
    let member = |name, peer_target, port| {
        let resources = vec![
            ("mirror_ep", peer(peer_target, "pub")),
            ("pub_ep", listen(port)),
        ];
        member(name, vec![mirror(peer_target), publish.clone()], resources)
    };
    let mut envelope = DeploymentConfig {
        config_version: CONFIG_VERSION,
        hosts: Default::default(),
        targets: vec![member("a", "b", 1), member("b", "a", 2)],
    };
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
fn test_params_naming_a_peer_are_untouched() {
    let mut envelope = envelope();
    envelope.targets[0].coordinator.systems[0].params =
        json!({ "transport": { "peer": { "target": "b", "link": "db" } } });
    let params = envelope.targets[0].coordinator.systems[0].params.clone();
    resolve(&mut envelope, &tables(2)).expect("resolves");
    assert!(!unresolved(&envelope).expect("decodes"));
    assert_eq!(envelope.targets[0].coordinator.systems[0].params, params);
}

#[test]
fn test_resolve_keeps_other_params() {
    let mut envelope = envelope();
    envelope.targets[1].coordinator.systems[0].params = json!({ "conn_cap": 64 });
    envelope.targets[1].coordinator.systems[1].params["path"] = json!("/db");
    resolve(&mut envelope, &tables(2)).expect("resolves");
    let [mirror, db] = &envelope.targets[1].coordinator.systems[..] else {
        panic!("two systems")
    };
    assert_eq!(mirror.params, json!({ "conn_cap": 64 }));
    assert_eq!(db.params["path"], "/db");
    assert_eq!(db.params["listen"], "127.0.0.1:2250");
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
    let pending = pending(&envelope).expect("decodes").expect("the peers");
    assert!(pending.slots.is_empty(), "a stored slot is not pending");
    let before = envelope.targets[0].coordinator.systems[4].params.clone();
    resolve(&mut envelope, &tables(2)).expect("resolves");
    assert_eq!(envelope.targets[0].coordinator.systems[4].params, before);
    assert!(!unresolved(&envelope).expect("decodes"));
}

#[test]
fn test_first_v4_prefers_ipv4() {
    let v6: SocketAddr = "[::1]:1".parse().expect("an address");
    let v4: SocketAddr = "127.0.0.1:1".parse().expect("an address");
    assert_eq!(first_v4(&[v6, v4]), Some(v4));
    assert_eq!(first_v4(&[v6]), Some(v6));
    assert_eq!(first_v4(&[]), None);
}
