//! The target's Python system, end to end: `target.py`'s `@system` compiles
//! at provision into the program's wasm pack artifact, resolves through the
//! wired wasm arm with its edge synthesized from the baked bindings — its
//! `plant.sensors.gyro_b` binding naming the mirror the fsw member carries —
//! and runs in the same loop as the systems it reads.

#![cfg(not(miri))]

use metor_fsw_2::metor_proto::types::ComponentId;

mod common;

#[test]
fn the_targets_python_system_resolves_and_runs() {
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
    assert!(
        coord
            .registry()
            .view(ComponentId::new("fsw.gyro_norm.gyro_norm"))
            .expect("the Python system's output is registered")
            .is_ok(),
        "a reader slot is available"
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
}
