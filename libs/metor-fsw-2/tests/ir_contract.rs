//! The [`Wiring`] IR's JSON representation is the cross-language contract the
//! `metor_config` Python emitter is written against. serde's rendering —
//! externally tagged enums, exact field names, `Option` as `null` — is the
//! source of truth; the Python side conforms to it, never the reverse.
//!
//! Two checks anchor that contract: a maximal `Wiring` that round-trips through
//! JSON unchanged (and whose rendering pins the enum tagging), and the shared
//! `tests/golden/target.json` fixture — the same file the Python golden test
//! consumes — deserialized and re-serialized to prove it is exactly what Rust
//! accepts and emits.

use std::collections::BTreeMap;

use metor_fsw_2::ir::{
    ArtifactKind, Deployment, EdgeKind, IR_VERSION, PeerSpec, ScopeSpec, SourceRef,
};
use metor_fsw_2::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, DistRef, EdgeSpec,
    InitialOccupantSpec, ParamSource, ProgramDecl, ProgramSpec, SlotInitState, SlotSpec,
    SystemSpec, Wiring,
};
use serde_json::{Value, json};

/// A `Wiring` exercising every spec kind, all three `ParamSource` variants, both
/// edge kinds with and without `delayed`, a nested scope table, and source
/// anchors — the maximal shape the representation must survive.
fn maximal() -> Wiring {
    let src = |line| {
        Some(SourceRef {
            file: Some("target.py".into()),
            line,
            col: 1,
        })
    };
    Wiring {
        ir_version: IR_VERSION,
        coordinator: CoordinatorSpec {
            cycle_rate: 120.0,
            default_depth: Some(8),
            clock: ClockSpec::Simulated { dt_secs: 0.5 },
            namespace: None,
            wasm_fuel_per_poll: Some(50_000_000),
            wasm_memory_limit_bytes: Some(32 * 1024 * 1024),
        },
        artifacts: vec![
            Artifact {
                kind: Default::default(),
                id: "adcs".into(),
                crate_name: "adcs-systems".into(),
                lib: "adcs_systems".into(),
                path: None,
                prebuilt_dir: None,
                dist: None,
                manifest_hash: None,
                src: src(3),
            },
            Artifact {
                kind: Default::default(),
                id: "gnc".into(),
                crate_name: "gnc-systems".into(),
                lib: "gnc_systems".into(),
                path: None,
                prebuilt_dir: Some("/venv/gnc_pack/_libs".into()),
                dist: Some(DistRef {
                    name: "gnc-pack".into(),
                    version: "1.2.0".into(),
                }),
                manifest_hash: Some("sha256:0".into()),
                src: None,
            },
            // The program-built wasm artifact: no crate, no lib stem, no
            // prebuilt dir — compiled from `Wiring::program` at provision.
            Artifact {
                kind: ArtifactKind::Wasm,
                id: "program".into(),
                crate_name: String::new(),
                lib: String::new(),
                path: None,
                prebuilt_dir: None,
                dist: None,
                manifest_hash: None,
                src: None,
            },
        ],
        states: Vec::new(),
        systems: vec![
            SystemSpec {
                name: "block.plant".into(),
                ty: Some("Plant".into()),
                artifact: Some("adcs".into()),
                params: ParamSource::Value(json!({ "init_angle": 0.5, "seed": 42 })),
                process: true,
                src: src(5),
                scope: Some(0),
                attach: None,
                layout: Some((40.0, 80.0)),
                status: Some("nav.system_status".into()),
                encompassing: true,
                peer: None,
            },
            SystemSpec {
                name: "postcard_sys".into(),
                ty: Some("Nav".into()),
                artifact: Some("adcs".into()),
                params: ParamSource::Postcard(vec![1, 2, 3, 4]),
                process: false,
                src: None,
                scope: Some(1),
                attach: None,
                layout: None,
                status: None,
                encompassing: false,
                peer: None,
            },
            SystemSpec {
                name: "bare".into(),
                ty: None,
                artifact: None,
                params: ParamSource::None,
                process: false,
                src: None,
                scope: None,
                attach: Some("link".into()),
                layout: None,
                status: None,
                encompassing: false,
                peer: None,
            },
            // A registered `@system`: an ordinary spec, free to sit anywhere
            // in the list, carry a scope, and name its instance apart from
            // the declaration its `ty` addresses.
            SystemSpec {
                name: "block.rate_mag".into(),
                ty: Some("omega_norm".into()),
                artifact: Some("program".into()),
                params: ParamSource::None,
                process: false,
                src: src(12),
                scope: Some(0),
                attach: None,
                layout: Some((420.0, 180.0)),
                status: None,
                encompassing: false,
                peer: None,
            },
            // A mirror: the peer type and artifact, no params of its own, and
            // the `peer` spec that makes the subscriber its implementation.
            SystemSpec {
                name: "sim".into(),
                ty: Some("Plant".into()),
                artifact: Some("adcs".into()),
                params: ParamSource::None,
                src: src(20),
                process: false,
                scope: None,
                attach: None,
                layout: None,
                status: None,
                encompassing: false,
                peer: Some(PeerSpec {
                    namespace: "plant".into(),
                    link: "peer".into(),
                    port: 2242,
                    instance: "plant".into(),
                    telemetered: true,
                    host: None,
                }),
            },
        ],
        slots: vec![SlotSpec {
            name: "mode".into(),
            inputs: vec!["attitude_estimate".into(), "gps".into()],
            outputs: vec!["mode_cmd".into()],
            allow: vec![
                AllowedOccupantSpec {
                    occupant: "commissioning".into(),
                    artifact: Some("seqs".into()),
                    params: ParamSource::Value(json!({ "rate": 1.0 })),
                    src: src(9),
                },
                AllowedOccupantSpec {
                    occupant: "safe_mode".into(),
                    artifact: None,
                    params: ParamSource::None,
                    src: None,
                },
            ],
            initial: Some(InitialOccupantSpec {
                occupant: "commissioning".into(),
                state: SlotInitState::Running,
            }),
            process: false,
            src: src(8),
            scope: Some(0),
            status: Some("mode.system_status".into()),
        }],
        edges: vec![
            EdgeSpec {
                from: "block.plant".into(),
                out: "sensors".into(),
                to: "postcard_sys".into(),
                in_: "sensors".into(),
                delayed: false,
                kind: EdgeKind::Frame,
                src: None,
            },
            EdgeSpec {
                from: "mode".into(),
                out: "mode_cmd".into(),
                to: "block.plant".into(),
                in_: "torque_cmd".into(),
                delayed: true,
                kind: EdgeKind::Frame,
                src: None,
            },
            EdgeSpec {
                from: "coordinator".into(),
                out: "SequenceCommand".into(),
                to: "mode".into(),
                in_: "SequenceCommand".into(),
                delayed: false,
                kind: EdgeKind::Msg,
                src: None,
            },
        ],
        scopes: vec![
            ScopeSpec {
                path: "block".into(),
                parent: None,
                src: src(4),
            },
            ScopeSpec {
                path: "block.inner".into(),
                parent: Some(0),
                src: None,
            },
        ],
        program: Some(ProgramSpec {
            source: "@system(\"block.plant.sensors.gyro_b\")\ndef omega_norm(gyro_b) -> f64:\n    \
                     return (gyro_b @ gyro_b) ** 0.5\n\n"
                .into(),
            decls: vec![ProgramDecl {
                name: "omega_norm".into(),
                src: src(12),
                offset: 0,
            }],
        }),
    }
}

#[test]
fn maximal_wiring_round_trips() {
    let w = maximal();
    let json = serde_json::to_string(&w).unwrap();
    let back: Wiring = serde_json::from_str(&json).unwrap();
    assert_eq!(w, back, "a maximal Wiring must survive a JSON round-trip");
}

/// Pin the externally-tagged enum rendering and the exact field names the
/// Python emitter has to reproduce.
#[test]
fn representation_is_externally_tagged() {
    let v = serde_json::to_value(maximal()).unwrap();

    // Enums render externally tagged: unit variants as bare strings, struct/
    // newtype variants as a single-key object.
    assert_eq!(
        v["coordinator"]["clock"],
        json!({ "Simulated": { "dt_secs": 0.5 } })
    );
    assert_eq!(v["coordinator"]["wasm_fuel_per_poll"], json!(50_000_000));
    assert_eq!(
        v["coordinator"]["wasm_memory_limit_bytes"],
        json!(32 * 1024 * 1024)
    );
    assert_eq!(
        v["systems"][0]["params"],
        json!({ "Value": { "init_angle": 0.5, "seed": 42 } })
    );
    assert_eq!(v["systems"][2]["params"], json!("None"));
    assert_eq!(
        v["systems"][1]["params"],
        json!({ "Postcard": [1, 2, 3, 4] })
    );
    assert_eq!(v["slots"][0]["initial"]["state"], json!("Running"));
    assert!(
        serde_json::from_value::<SlotInitState>(json!("Empty")).is_err(),
        "an absent initial occupant is the only empty-slot representation"
    );
    assert_eq!(v["edges"][2]["kind"], json!("Msg"));
    assert_eq!(v["edges"][0]["kind"], json!("Frame"));

    // The consumer input port field is `in_`, not `in`.
    assert_eq!(v["edges"][0]["in_"], json!("sensors"));
    // Absent optionals render as null, not omitted.
    assert_eq!(v["systems"][2]["ty"], Value::Null);
    assert_eq!(v["systems"][2]["scope"], Value::Null);

    // Layout renders as a two-element array, absent as null.
    assert_eq!(v["systems"][0]["layout"], json!([40.0, 80.0]));
    assert_eq!(v["systems"][1]["layout"], Value::Null);

    // The run-record override is a plain dotted prefix string, and is omitted
    // entirely when unset — the shape every existing front end emits, so the
    // goldens and the Python emitter stay untouched.
    assert_eq!(v["systems"][0]["status"], json!("nav.system_status"));
    assert_eq!(v["slots"][0]["status"], json!("mode.system_status"));
    assert!(v["systems"][1].get("status").is_none());
    assert!(v["systems"][2].get("status").is_none());

    // The encompassing marker renders as a bare `true` and, like `status`,
    // vanishes when unset — a document written before it existed is unchanged.
    assert_eq!(v["systems"][0]["encompassing"], json!(true));
    assert!(v["systems"][1].get("encompassing").is_none());
    assert!(v["systems"][3].get("encompassing").is_none());

    // The captured program: a Python system is an ordinary spec addressing
    // the program artifact's pack entry by `ty`, its instance name and scope
    // the registering `add`'s own.
    assert_eq!(v["systems"][3]["name"], json!("block.rate_mag"));
    assert_eq!(v["systems"][3]["ty"], json!("omega_norm"));
    assert_eq!(v["systems"][3]["scope"], json!(0));
    assert_eq!(v["systems"][3]["artifact"], json!("program"));
    assert_eq!(v["systems"][3]["params"], json!("None"));
    assert_eq!(v["program"]["decls"][0]["name"], json!("omega_norm"));
    assert_eq!(v["program"]["decls"][0]["offset"], json!(0));

    // A mirror carries `peer` and nothing else new; `telemetered` is an
    // opt-out, so the common `true` never reaches the wire, and `host` is
    // filled by the CLI rather than a front end.
    assert_eq!(
        v["systems"][4]["peer"],
        json!({ "namespace": "plant", "link": "peer", "port": 2242, "instance": "plant" })
    );
    assert!(v["systems"][0].get("peer").is_none());

    // Artifact kind renders snake_case and is omitted for the default cdylib;
    // a program-built wasm artifact omits the crate/lib fields entirely.
    assert_eq!(v["artifacts"][2]["kind"], json!("wasm"));
    assert!(v["artifacts"][0].get("kind").is_none());
    assert!(v["artifacts"][2].get("crate_name").is_none());
    assert!(v["artifacts"][2].get("lib").is_none());

    // The v3 artifact fields: the arch-neutral lib stem, a prebuilt dir as a
    // plain path string, and dist provenance as a { name, version } object.
    assert_eq!(v["artifacts"][0]["lib"], json!("adcs_systems"));
    assert_eq!(v["artifacts"][0]["prebuilt_dir"], Value::Null);
    assert_eq!(v["artifacts"][0]["dist"], Value::Null);
    assert_eq!(
        v["artifacts"][1]["prebuilt_dir"],
        json!("/venv/gnc_pack/_libs")
    );
    assert_eq!(
        v["artifacts"][1]["dist"],
        json!({ "name": "gnc-pack", "version": "1.2.0" })
    );
}

/// The two v11 fields are additive: a v10 document carries neither, so it
/// deserializes once its version is bumped and reads as the defaults —
/// no `peer`, no `hosts`, and a mirror that is telemetered.
#[test]
fn a_v10_document_defaults_the_new_fields() {
    let mut v = serde_json::to_value(maximal()).unwrap();
    for system in v["systems"].as_array_mut().unwrap() {
        system.as_object_mut().unwrap().remove("peer");
    }
    let envelope = json!({ "ir_version": IR_VERSION, "targets": [v] });

    let d: Deployment = serde_json::from_value(envelope).unwrap();
    assert!(d.hosts.is_empty());
    assert!(d.targets[0].systems.iter().all(|s| s.peer.is_none()));

    let telemetered: PeerSpec = serde_json::from_value(
        json!({ "namespace": "plant", "link": "peer", "port": 2242, "instance": "plant" }),
    )
    .unwrap();
    assert!(telemetered.telemetered);
    assert_eq!(telemetered.host, None);
}

/// The shared fixture: the exact JSON the Python emitter must produce, minus
/// the build- and provenance-dependent fields both consumers normalize away
/// (`src` everywhere, and each artifact's located `path`/`prebuilt_dir`).
/// Deserializing then re-serializing proves the fixture is precisely what
/// Rust round-trips to.
#[test]
fn golden_fixture_round_trips() {
    const GOLDEN: &str = include_str!("golden/target.json");
    let w: Wiring = serde_json::from_str(GOLDEN).expect("golden fixture deserializes as Wiring");
    let reserialized = normalize(serde_json::to_value(&w).unwrap());
    let on_disk = normalize(serde_json::from_str(GOLDEN).unwrap());
    assert_eq!(
        reserialized, on_disk,
        "the golden fixture must equal its own Rust round-trip after normalization"
    );
}

/// The deployment envelope is the document both front-ends emit, so its JSON
/// must survive a round-trip with its members intact and in order.
#[test]
fn envelope_round_trips() {
    let member = |namespace: &str| {
        let mut w = maximal();
        w.coordinator.namespace = Some(namespace.into());
        w
    };
    let deployment = Deployment {
        ir_version: IR_VERSION,
        targets: vec![member("plant"), member("fsw")],
        hosts: BTreeMap::from([("plant".to_string(), "10.0.0.5".to_string())]),
    };

    let json = serde_json::to_string(&deployment).unwrap();
    let back: Deployment = serde_json::from_str(&json).unwrap();
    assert_eq!(back, deployment);
    assert_eq!(
        back.target(Some("fsw"))
            .unwrap()
            .coordinator
            .namespace
            .as_deref(),
        Some("fsw")
    );
}

/// The shared `tests/golden/deployment.json` fixture: the two-member envelope
/// the Python golden test also pins. Beyond the round-trip, it holds the two
/// facts a deployment adds over a `Wiring` — members compile only their own
/// Python systems, and a member's presets are qualified with its namespace.
#[test]
fn golden_deployment_round_trips() {
    const GOLDEN: &str = include_str!("golden/deployment.json");
    let d: Deployment =
        serde_json::from_str(GOLDEN).expect("golden fixture deserializes as Deployment");
    let reserialized = normalize(serde_json::to_value(&d).unwrap());
    let on_disk = normalize(serde_json::from_str(GOLDEN).unwrap());
    assert_eq!(
        reserialized, on_disk,
        "the golden deployment must equal its own Rust round-trip after normalization"
    );

    let decls = |w: &Wiring| {
        w.program
            .as_ref()
            .map(|p| p.decls.iter().map(|d| d.name.clone()).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let plant = d.target(Some("plant")).unwrap();
    let fsw = d.target(Some("fsw")).unwrap();
    assert!(
        decls(plant).iter().all(|n| !decls(fsw).contains(n)),
        "a member's program carries only the systems it added"
    );
    assert_eq!(decls(fsw), ["gyro_norm"]);

    let presets = fsw
        .systems
        .iter()
        .find(|s| s.ty.as_deref() == Some("Presets"))
        .expect("the fsw member broadcasts presets");
    let ParamSource::Value(params) = &presets.params else {
        panic!("presets carry inline params")
    };
    let id = metor_proto::types::ComponentId::new("fsw.adcs.gyro_norm").0;
    assert!(
        serde_json::to_string(params)
            .unwrap()
            .contains(&id.to_string()),
        "preset component ids are qualified with the member's namespace"
    );
}

/// Strip the fields the cross-language comparison ignores: every `src` anchor
/// (line numbers track the emitting source), the envelope's emitter-only
/// `metor_config_version`, and each artifact's `path` and `prebuilt_dir` (both
/// machine-located). The `lib` stem is arch-neutral and stays in the
/// comparison.
fn normalize(mut v: Value) -> Value {
    strip_key(&mut v, "src");
    strip_key(&mut v, "metor_config_version");
    if let Some(artifacts) = v.get_mut("artifacts").and_then(Value::as_array_mut) {
        for a in artifacts {
            if let Some(obj) = a.as_object_mut() {
                obj.remove("path");
                obj.remove("prebuilt_dir");
            }
        }
    }
    v
}

fn strip_key(v: &mut Value, key: &str) {
    match v {
        Value::Object(map) => {
            map.remove(key);
            for child in map.values_mut() {
                strip_key(child, key);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|c| strip_key(c, key)),
        _ => {}
    }
}
