use metor_fsw_2::ir::{ClockSpec, CoordinatorSpec, IR_VERSION, ParamSource, SystemSpec, Wiring};
use metor_proto::types::Timestamp;
use metor_proto_wkt::WiringManifest;

use super::WiringState;

/// A minimal one-system wiring whose single system carries `name`, used to
/// tell one synthetic manifest from another.
fn wiring_with_system(name: &str) -> Wiring {
    Wiring {
        ir_version: IR_VERSION,
        coordinator: CoordinatorSpec {
            cycle_rate: 100.0,
            default_depth: None,
            clock: ClockSpec::Wall,
            namespace: None,
            wasm_fuel_per_poll: None,
            wasm_memory_limit_bytes: None,
        },
        artifacts: Vec::new(),
        states: Vec::new(),
        systems: vec![SystemSpec {
            name: name.to_string(),
            ty: Some("Demo".to_string()),
            artifact: None,
            params: ParamSource::None,
            process: false,
            src: None,
            scope: None,
            attach: None,
        }],
        slots: Vec::new(),
        edges: Vec::new(),
        scopes: Vec::new(),
    }
}

fn manifest_for(wiring: &Wiring) -> WiringManifest {
    WiringManifest {
        ir_version: wiring.ir_version,
        ir_json: serde_json::to_string(wiring).expect("serialize fixture wiring"),
    }
}

#[test]
fn fold_replaces_on_reemit() {
    let mut state = WiringState::default();
    state.apply(Timestamp(1), manifest_for(&wiring_with_system("alpha")));
    assert_eq!(state.wiring().unwrap().systems[0].name, "alpha");
    assert!(state.error().is_none());

    state.apply(Timestamp(2), manifest_for(&wiring_with_system("beta")));
    let w = state.wiring().unwrap();
    assert_eq!(w.systems.len(), 1);
    assert_eq!(w.systems[0].name, "beta");
    assert!(state.error().is_none());
    assert_eq!(state.updated_at(), Some(Timestamp(2)));
}

#[test]
fn bad_json_held_as_error_keeps_prior_topology() {
    let mut state = WiringState::default();
    state.apply(Timestamp(1), manifest_for(&wiring_with_system("alpha")));

    state.apply(
        Timestamp(2),
        WiringManifest {
            ir_version: IR_VERSION,
            ir_json: "{ this is not valid json".to_string(),
        },
    );
    assert!(state.error().is_some(), "bad JSON must surface an error");
    // Previous good topology is retained rather than dropped.
    assert_eq!(state.wiring().unwrap().systems[0].name, "alpha");
}

#[test]
fn version_mismatch_surfaced_as_error() {
    let mut state = WiringState::default();
    state.apply(
        Timestamp(1),
        WiringManifest {
            ir_version: IR_VERSION + 1,
            ir_json: "{}".to_string(),
        },
    );
    let err = state
        .error()
        .expect("version mismatch must surface an error");
    assert!(err.contains(&(IR_VERSION + 1).to_string()));
    assert!(
        state.wiring().is_none(),
        "no topology from an unsupported version"
    );
}

#[test]
fn good_manifest_after_error_clears_it() {
    let mut state = WiringState::default();
    state.apply(
        Timestamp(1),
        WiringManifest {
            ir_version: IR_VERSION,
            ir_json: "nonsense".to_string(),
        },
    );
    assert!(state.error().is_some());

    state.apply(Timestamp(2), manifest_for(&wiring_with_system("gamma")));
    assert!(
        state.error().is_none(),
        "a good manifest clears the held error"
    );
    assert_eq!(state.wiring().unwrap().systems[0].name, "gamma");
}
