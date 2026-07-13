//! The [`Wiring`] IR's JSON representation is the cross-language contract the
//! `metor_config` Python emitter is written against. serde's rendering —
//! externally tagged enums, exact field names, `Option` as `null` — is the
//! source of truth; the Python side conforms to it, never the reverse.
//!
//! Two checks anchor that contract: a maximal `Wiring` that round-trips through
//! JSON unchanged (and whose rendering pins the enum tagging), and the shared
//! `tests/golden/mission.json` fixture — the same file the Python golden test
//! consumes — deserialized and re-serialized to prove it is exactly what Rust
//! accepts and emits.

#![cfg(feature = "wiring")]

use metor_fsw_2::ir::{EdgeKind, IR_VERSION, ScopeSpec, SourceRef};
use metor_fsw_2::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeSpec, InitialOccupantSpec,
    ParamSource, SlotInitState, SlotSpec, SystemSpec, Wiring,
};
use serde_json::{Value, json};

/// A `Wiring` exercising every spec kind, all three `ParamSource` variants, both
/// edge kinds with and without `delayed`, a nested scope table, and source
/// anchors — the maximal shape the representation must survive.
fn maximal() -> Wiring {
    let src = |line| {
        Some(SourceRef {
            file: Some("mission.py".into()),
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
        },
        artifacts: vec![Artifact {
            id: "adcs".into(),
            crate_name: "adcs-systems".into(),
            cdylib: "libadcs_systems.dylib".into(),
            path: None,
            manifest_hash: None,
            src: src(3),
        }],
        systems: vec![
            SystemSpec {
                name: "block.plant".into(),
                ty: Some("Plant".into()),
                artifact: Some("adcs".into()),
                params: ParamSource::Value(json!({ "init_angle": 0.5, "seed": 42 })),
                process: true,
                src: src(5),
                scope: Some(0),
            },
            SystemSpec {
                name: "postcard_sys".into(),
                ty: Some("Nav".into()),
                artifact: Some("adcs".into()),
                params: ParamSource::Postcard(vec![1, 2, 3, 4]),
                process: false,
                src: None,
                scope: Some(1),
            },
            SystemSpec {
                name: "bare".into(),
                ty: None,
                artifact: None,
                params: ParamSource::None,
                process: false,
                src: None,
                scope: None,
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
    assert_eq!(v["edges"][2]["kind"], json!("Msg"));
    assert_eq!(v["edges"][0]["kind"], json!("Frame"));

    // The consumer input port field is `in_`, not `in`.
    assert_eq!(v["edges"][0]["in_"], json!("sensors"));
    // Absent optionals render as null, not omitted.
    assert_eq!(v["systems"][2]["ty"], Value::Null);
    assert_eq!(v["systems"][2]["scope"], Value::Null);
}

/// The shared fixture: the exact JSON the Python emitter must produce, minus
/// the build- and provenance-dependent fields both consumers normalize away
/// (`src` everywhere, and each artifact's platform `cdylib` and located
/// `path`). Deserializing then re-serializing proves the fixture is precisely
/// what Rust round-trips to.
#[test]
fn golden_fixture_round_trips() {
    const GOLDEN: &str = include_str!("golden/mission.json");
    let w: Wiring = serde_json::from_str(GOLDEN).expect("golden fixture deserializes as Wiring");
    let reserialized = normalize(serde_json::to_value(&w).unwrap());
    let on_disk = normalize(serde_json::from_str(GOLDEN).unwrap());
    assert_eq!(
        reserialized, on_disk,
        "the golden fixture must equal its own Rust round-trip after normalization"
    );
}

/// Strip the fields the cross-language comparison ignores: every `src` anchor
/// (line numbers track the emitting source) and each artifact's `cdylib`
/// (platform-specific) and `path` (build-located).
pub fn normalize(mut v: Value) -> Value {
    strip_key(&mut v, "src");
    if let Some(artifacts) = v.get_mut("artifacts").and_then(Value::as_array_mut) {
        for a in artifacts {
            if let Some(obj) = a.as_object_mut() {
                obj.remove("cdylib");
                obj.remove("path");
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
