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
        log: None,
        grants: Vec::new(),
    }
}

fn slot_def(params: &Value) -> Result<SystemDef, DefError> {
    DlSlot::def(&DefCx {
        params,
        ..DefCx::empty()
    })
}

fn candidate(path: &str) -> Candidate {
    Candidate {
        name: "up".into(),
        ty: "count_up".into(),
        params: Value::Null,
        stored: Stored {
            path: path.into(),
            hash: blake3::hash(b"lib"),
            def: def_with(Vec::new()),
            kind: PackKind::Dylib,
        },
    }
}

fn stored(name: &str, def: &SystemDef) -> Value {
    let hash = blake3::hash(b"lib").to_hex();
    json!({ "name": name, "ty": name, "stored": {
        "path": "/store/x/lib.so", "hash": hash.as_str(), "def": def,
    }})
}

fn from_pack(name: &str) -> Value {
    let pack = json!({ "id": "echo", "lib": "echo_pack", "libs": "/l" });
    json!({ "name": name, "ty": name, "pack": pack })
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
fn test_pack_candidate_does_not_parse() {
    let later = json!({ "allow": [stored("up", &def_with(Vec::new())), from_pack("down")] });
    for params in [json!({ "allow": [from_pack("up")] }), later] {
        let Err(DefError::Slot { message }) = slot_def(&params) else {
            panic!("a pack candidate is not a runtime candidate")
        };
        assert!(message.contains("`stored`"), "{message}");
    }
}

#[test]
fn test_stored_hash_parses_as_hex() {
    let hash = blake3::hash(b"lib");
    let text = serde_json::to_value(candidate("/store/x/libup.so")).expect("encodes");
    assert_eq!(text["stored"]["hash"], json!(hash.to_hex().as_str()));
    let mut bad = stored("up", &def_with(Vec::new()));
    bad["stored"]["hash"] = json!("not hex");
    assert!(serde_json::from_value::<Candidate>(bad).is_err());
}

#[test]
fn test_def_refuses_empty_or_malformed() {
    assert!(slot_def(&json!({ "allow": [] })).is_err());
    assert!(slot_def(&json!({ "allow": 3 })).is_err());
    assert!(slot_def(&Value::Null).is_err());
    let unstored = json!({ "allow": [{ "name": "up", "ty": "count_up" }] });
    assert!(slot_def(&unstored).is_err());
}

#[test]
fn test_resolved_occupant() {
    let hash = blake3::hash(b"lib");
    let resolved = Resolved::of(2, candidate("/store/x/libup.so")).expect("a resolved candidate");
    assert_eq!(resolved.occupant.index, 2);
    assert_eq!(resolved.occupant.artifact.to_str(), Ok("libup.so"));
    assert_eq!(resolved.occupant.hash.0, *hash.as_bytes());
}

#[test]
fn test_resolved_refuses_bad_file_names() {
    let long = format!("/store/x/{}", "l".repeat(64));
    assert!(Resolved::of(0, candidate(&long)).is_err());
    assert!(Resolved::of(0, candidate("/")).is_err());
}

#[test]
fn test_same_ports() {
    let a = def_with(vec![port("imu")]);
    assert!(a.same_ports(&a));
    assert!(!a.same_ports(&def_with(vec![port("other")])));
    assert!(!a.same_ports(&def_with(Vec::new())));
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
    assert!(a.same_ports(&rooted));
    let capped = SystemDef {
        grants: vec![crate::io::GrantDef {
            param: "uart".into(),
            interface: "uart".into(),
        }],
        ..a.clone()
    };
    assert!(!a.same_ports(&capped), "candidates take the same caps");
}
