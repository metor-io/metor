//! The target's Python system, end to end: `target.py`'s `@system` compiles
//! at provision into the program's wasm pack artifact, resolves through the
//! wired wasm arm with its edge synthesized from the baked bindings, runs in
//! the same loop as the dlopen'd plant it reads — and its telemetry agrees
//! with the nox oracle over the plant's own published measurements.

#![cfg(not(miri))]

use metor_fsw_2::Input;
use metor_fsw_2::metor_proto::types::{ComponentId, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use adcs_contracts::Sensors;

mod common;

/// Host mirror of the compiled system's one-field output frame: an 8-byte
/// timestamp then the eight-byte slot, the layout the compiler documents.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "gyro_norm")]
struct GyroNorm {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    gyro_norm: f64,
}

#[test]
fn the_targets_python_system_matches_the_nox_oracle() {
    if !common::ensure_stubs() {
        return;
    }
    let _guard = common::link_port_guard();
    let mut coord = match adcs_fsw2::build_sim_coordinator() {
        Ok(coord) => coord,
        Err(e) => {
            eprintln!("skipping: build_sim_coordinator failed: {e}");
            return;
        }
    };

    let mut sensors: Input<Sensors> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("cube_sat.plant.sensors"))
            .expect("plant.sensors is registered")
            .expect("a reader slot is available"),
    );
    let mut norms: Input<GyroNorm> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("cube_sat.gyro_norm.gyro_norm"))
            .expect("the Python system's output is registered")
            .expect("a reader slot is available"),
    );

    let coord = stellarator::run(|| async move {
        coord.run_for(60).await;
        coord
    });
    assert!(
        coord.stopped().is_empty(),
        "nothing hard-stopped: {:?}",
        coord.stopped()
    );

    // Match records by timestamp: the synthesized edge is same-cycle, so a
    // norm record pairs with the sensors record of the same stamp.
    let mut measured: Vec<(Timestamp, adcs_contracts::V3)> = Vec::new();
    sensors
        .drain(|record| {
            measured.push((record.get().timestamp, record.get().gyro_b));
        })
        .expect("sensors ring intact");
    let mut checked = 0;
    norms
        .drain(|record| {
            let ts = record.get().timestamp;
            let Some((_, gyro)) = measured.iter().find(|(t, _)| *t == ts) else {
                return;
            };
            let oracle: f64 = gyro.norm().into_buf();
            let got = record.get().gyro_norm;
            assert!(
                (got - oracle).abs() <= 1e-12 * oracle.abs().max(1.0),
                "norm at {ts:?}: compiled {got} vs nox {oracle}"
            );
            checked += 1;
        })
        .expect("norm ring intact");
    assert!(
        checked >= 5,
        "enough paired samples to mean something (got {checked})"
    );
}
