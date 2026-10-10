use serde_json::json;

use super::*;
use crate::Output;
use crate::tests::utils::Imu;

fn port(name: &'static str) -> PortDef {
    Output::<Imu>::def(&DefCx::empty(), name)
}

fn def_with(outputs: Vec<PortDef>) -> SystemDef {
    SystemDef {
        name: "count_up".into(),
        inputs: Vec::new(),
        outputs,
        grants: Vec::new(),
    }
}

fn slot_def(params: &Value) -> Result<SystemDef, DefError> {
    def(&DefCx {
        params,
        ..DefCx::empty()
    })
}

fn candidate(path: &str, hash: &str) -> Candidate {
    Candidate {
        name: "up".into(),
        ty: "count_up".into(),
        params: Value::Null,
        source: Source::Stored(Stored {
            path: path.into(),
            hash: hash.into(),
            def: def_with(Vec::new()),
            kind: PackKind::Dylib,
            limits: Limits::default(),
        }),
    }
}

fn stored(name: &str, def: &SystemDef) -> Value {
    json!({ "name": name, "ty": name, "source": { "stored": {
        "path": "/store/x/lib.so", "hash": "00", "def": def,
    }}})
}

fn from_pack(name: &str) -> Value {
    let pack = json!({ "id": "echo", "lib": "echo_pack", "libs": "/l" });
    json!({ "name": name, "ty": name, "source": { "pack": pack } })
}

#[test]
fn test_def_is_first_candidate() {
    let baked = def_with(vec![port("imu")]);
    let params = json!({ "allow": [
        stored("up", &baked),
        stored("down", &def_with(Vec::new())),
    ]});
    let def = slot_def(&params).expect("a baked def");
    assert_eq!(def.name, SLOT_NAME);
    assert_eq!(def.outputs, baked.outputs);
}

#[test]
fn test_def_refuses_unresolved() {
    let params = json!({ "allow": [from_pack("up")] });
    let Err(DefError::Slot { message }) = slot_def(&params) else {
        panic!("an unresolved candidate has no def")
    };
    assert!(message.contains("metor build"), "{message}");
}

#[test]
fn test_def_refuses_later_unresolved() {
    let params = json!({ "allow": [stored("up", &def_with(Vec::new())), from_pack("down")] });
    let Err(DefError::Slot { message }) = slot_def(&params) else {
        panic!("every candidate must be resolved")
    };
    assert!(message.contains("`down`"), "{message}");
    assert!(message.contains("metor build"), "{message}");
}

#[test]
fn test_def_refuses_empty_or_malformed() {
    assert!(slot_def(&json!({ "allow": [] })).is_err());
    assert!(slot_def(&json!({ "allow": 3 })).is_err());
    assert!(slot_def(&Value::Null).is_err());
    let sourceless = json!({ "allow": [{ "name": "up", "ty": "count_up" }] });
    assert!(slot_def(&sourceless).is_err());
}

#[test]
fn test_resolved_occupant() {
    let hash = blake3::hash(b"lib");
    let resolved = Resolved::of(2, candidate("/store/x/libup.so", &hash.to_hex()))
        .expect("a resolved candidate");
    assert_eq!(resolved.occupant.index, 2);
    assert_eq!(resolved.occupant.artifact.to_str(), Ok("libup.so"));
    assert_eq!(resolved.occupant.hash.0, *hash.as_bytes());
}

#[test]
fn test_resolved_refuses_bad_fields() {
    let hash = blake3::hash(b"lib").to_hex();
    let long = format!("/store/x/{}", "l".repeat(64));
    assert!(Resolved::of(0, candidate("/store/x/libup.so", "not hex")).is_err());
    assert!(Resolved::of(0, candidate(&long, &hash)).is_err());
    let unresolved = Candidate {
        source: serde_json::from_value(json!({ "pack": { "id": "a", "lib": "a", "libs": "/l" } }))
            .expect("a pack source"),
        ..candidate("/store/x/libup.so", &hash)
    };
    assert!(Resolved::of(0, unresolved).is_err());
}

#[test]
fn test_same_ports() {
    let a = def_with(vec![port("imu")]);
    assert!(same_ports(&a, &a));
    assert!(!same_ports(&a, &def_with(vec![port("other")])));
    assert!(!same_ports(&a, &def_with(Vec::new())));
    let rooted = SystemDef {
        outputs: vec![Output::<Imu>::def(
            &DefCx {
                id: "mode",
                namespace: Some("sat"),
                ..DefCx::empty()
            },
            "imu",
        )],
        ..a.clone()
    };
    assert!(same_ports(&a, &rooted));
    let capped = SystemDef {
        grants: vec![crate::io::GrantDef {
            param: "uart".into(),
            interface: "uart".into(),
            end: crate::io::End::Host,
        }],
        ..a.clone()
    };
    assert!(!same_ports(&a, &capped), "candidates take the same caps");
}
