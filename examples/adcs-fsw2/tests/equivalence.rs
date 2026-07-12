//! The two front-ends agree: evaluating `mission.py` through the subprocess
//! CPython path yields a `Wiring` equivalent to parsing `mission.kdl`.
//!
//! Equivalence ignores what only one front-end carries — `src` anchors and the
//! scope table (the KDL front-end fills neither) — and compares params through
//! each front-end's own path (`ParamSource::Kdl` re-parsed, `ParamSource::Value`
//! as-is) as JSON value trees, with numbers compared by value so an integer and
//! its float twin match. Everything else — coordinator, artifacts, system order,
//! slot spec, and edges including `delayed` and `kind` — compares structurally.

#![cfg(not(miri))]

use std::path::{Path, PathBuf};
use std::process::Command;

use metor_fsw_2::wiring::{
    AllowedOccupantSpec, EdgeSpec, ParamNode, ParamSource, SlotSpec, SystemSpec, Wiring,
    eval_python_mission, param_value_tree, parse_with_origin,
};
use serde_json::Value;

fn mission(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn have_python() -> bool {
    Command::new("python3")
        .args([
            "-c",
            "import sys;sys.exit(0 if sys.version_info[:2]>=(3,10) else 1)",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Two JSON values are equal treating any two numbers with the same `f64` value
/// as equal, so a KDL integer literal and a Python float match.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(l, r)| json_eq(l, r))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, l)| y.get(k).is_some_and(|r| json_eq(l, r)))
        }
        _ => a == b,
    }
}

/// Compare two params through each front-end's path, as value trees.
fn params_eq(a: &ParamSource, b: &ParamSource, node: ParamNode) -> bool {
    let av = param_value_tree(a, node).expect("params render as a value tree");
    let bv = param_value_tree(b, node).expect("params render as a value tree");
    match (av, bv) {
        (None, None) => true,
        (Some(x), Some(y)) => json_eq(&x, &y),
        _ => false,
    }
}

fn systems_eq(a: &SystemSpec, b: &SystemSpec) -> bool {
    a.name == b.name
        && a.ty == b.ty
        && a.artifact == b.artifact
        && a.process == b.process
        && params_eq(&a.params, &b.params, ParamNode::System)
}

fn occupants_eq(a: &AllowedOccupantSpec, b: &AllowedOccupantSpec) -> bool {
    a.occupant == b.occupant
        && a.artifact == b.artifact
        && params_eq(&a.params, &b.params, ParamNode::Occupant)
}

fn slots_eq(a: &SlotSpec, b: &SlotSpec) -> bool {
    a.name == b.name
        && a.inputs == b.inputs
        && a.outputs == b.outputs
        && a.process == b.process
        && a.initial == b.initial
        && a.allow.len() == b.allow.len()
        && a.allow
            .iter()
            .zip(&b.allow)
            .all(|(x, y)| occupants_eq(x, y))
}

fn edges_eq(a: &EdgeSpec, b: &EdgeSpec) -> bool {
    a.from == b.from
        && a.out == b.out
        && a.to == b.to
        && a.in_ == b.in_
        && a.delayed == b.delayed
        && a.kind == b.kind
}

#[test]
fn python_and_kdl_missions_are_equivalent() {
    if !have_python() {
        eprintln!("skipping equivalence test: no python3 >= 3.10 on PATH");
        return;
    }

    let py: Wiring = eval_python_mission(&mission("mission.py")).expect("mission.py evaluates");
    let kdl_text = std::fs::read_to_string(mission("mission.kdl")).unwrap();
    let kdl: Wiring =
        parse_with_origin(&kdl_text, Some("mission.kdl")).expect("mission.kdl parses");

    assert_eq!(py.ir_version, kdl.ir_version);

    // Coordinator: cycle_rate and dt_secs are f64 read from the same decimals.
    assert!(json_eq(
        &serde_json::to_value(&py.coordinator).unwrap(),
        &serde_json::to_value(&kdl.coordinator).unwrap(),
    ));

    // Artifacts: id/crate_name/cdylib/path all structural (same platform).
    assert_eq!(py.artifacts.len(), kdl.artifacts.len());
    for (p, k) in py.artifacts.iter().zip(&kdl.artifacts) {
        assert_eq!(
            (&p.id, &p.crate_name, &p.cdylib, &p.path),
            (&k.id, &k.crate_name, &k.cdylib, &k.path)
        );
    }

    assert_eq!(py.systems.len(), kdl.systems.len(), "system count / order");
    for (p, k) in py.systems.iter().zip(&kdl.systems) {
        assert!(systems_eq(p, k), "system {} differs", p.name);
    }

    assert_eq!(py.slots.len(), kdl.slots.len());
    for (p, k) in py.slots.iter().zip(&kdl.slots) {
        assert!(slots_eq(p, k), "slot {} differs", p.name);
    }

    assert_eq!(py.edges.len(), kdl.edges.len(), "edge count / order");
    for (p, k) in py.edges.iter().zip(&kdl.edges) {
        assert!(edges_eq(p, k), "edge {}->{} differs", p.from, p.to);
    }
}
