//! The `adcs-fsw2` fsw member's `mode` **slot** running a real async-fn pack
//! occupant through the full `dlopen` path.
//!
//! Built from the SAME `target.py` the CLI runner consumes (evaluate →
//! `provision_artifacts` → `resolve`, as `bundle.rs` sets up — the sequence
//! cdylibs are built by `provision_artifacts` like any artifact). The slot
//! starts empty, so the scenario is the interactive one: `Load` → `Start` →
//! `Abort` through [`Coordinator::control_handle`]; the occupant folds the
//! cancel at its next `wait`, writes `ModeCmd::safe`, and ends `Aborted`. It
//! reads no plant data, so it holds on the fsw member alone. Gated off `miri`
//! (it builds + `dlopen`s real cdylibs).

#![cfg(not(miri))]

use std::cell::RefCell;
use std::rc::Rc;

use std::path::Path;

use adcs_contracts::ModeCmd;
use metor_fsw_2::metor_proto::types::ComponentId;
use metor_fsw_2::metor_proto_wkt::{SequenceCommand, SequenceCommandKind};
use metor_fsw_2::wiring::Registry;
use metor_fsw_2::wiring::{eval_python_deployment, provision_artifacts, resolve};
use metor_fsw_2::{BuildOptions, Coordinator, Input, SequenceStatus};

/// The target file — the same one the CLI runner and the other tests read.
fn target_py() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target.py")
}

mod common;

// `SequenceStatus::run_state` codes (sequence/mod.rs): 0 running, then `Outcome::run_state`.
const ABORTED: u8 = 2;

/// Build the fsw member off `target.py`. `None` if the build plumbing is
/// unavailable (so the caller skips rather than fails spuriously, like
/// `bundle`).
fn build_coordinator() -> Option<Coordinator> {
    if !common::ensure_stubs() {
        return None;
    }
    let deployment = match eval_python_deployment(&target_py()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping: target.py did not evaluate: {e}");
            return None;
        }
    };
    let mut wiring = deployment
        .target(Some("fsw"))
        .expect("the fsw member")
        .clone();
    // Test binaries can't host a `process=#true` worker (no `worker_entry` in main) — run
    // every system in-process, like `build_sim_coordinator`.
    for spec in &mut wiring.systems {
        spec.process = false;
    }
    if let Err(e) = provision_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: provision_artifacts failed: {e}");
        return None;
    }
    Some(resolve(&wiring, &Registry::with_builtins()).expect("resolve the target"))
}

/// Tap the slot's occupant `SequenceStatus` (`mode.sequence`) and its `ModeCmd` output
/// (`mode.mode_cmd`) from the coordinator registry.
fn tap_slot(coord: &mut Coordinator) -> (Input<SequenceStatus>, Input<ModeCmd>) {
    let seq = Input::new(
        coord
            .registry()
            .view(ComponentId::new("fsw.mode.sequence"))
            .expect("the slot's SequenceStatus is registered")
            .expect("a reader slot is available"),
    );
    let mode = Input::new(
        coord
            .registry()
            .view(ComponentId::new("fsw.mode.mode_cmd"))
            .expect("the slot's mode_cmd output is registered")
            .expect("a reader slot is available"),
    );
    (seq, mode)
}

/// A per-cycle sampler over the slot taps: the freshest `run_state` each cycle (`latest`, so
/// it never laps) and every distinct `ModeCmd.mode` published (`drain`, 1:1 with the cycle).
/// Returns `(run_states, modes)` captured over the run.
type Captured = (Rc<RefCell<Vec<u8>>>, Rc<RefCell<Vec<u8>>>);
fn spawn_sampler(
    seq: Input<SequenceStatus>,
    mode: Input<ModeCmd>,
) -> (Captured, stellarator::JoinHandle<()>) {
    let run_states = Rc::new(RefCell::new(Vec::<u8>::new()));
    let modes = Rc::new(RefCell::new(Vec::<u8>::new()));
    let (rs, ms) = (run_states.clone(), modes.clone());
    let handle = stellarator::spawn(async move {
        let (mut seq, mut mode) = (seq, mode);
        loop {
            stellarator::yield_now().await;
            if let Ok(Some(r)) = seq.latest() {
                rs.borrow_mut().push(r.get().run_state);
            }
            let _ = mode.drain(|f| ms.borrow_mut().push(f.get().mode));
        }
    });
    ((run_states, modes), handle)
}

#[test]
fn interactive_load_then_abort_safes() {
    let _guard = common::link_port_guard();
    let Some(mut coord) = build_coordinator() else {
        return;
    };
    let (seq, mode) = tap_slot(&mut coord);
    let mut control = coord.control_handle().expect("taken once per coordinator");

    // `run_for` re-runs the dl systems' (non-idempotent) `init` each call, so the slot is
    // driven inside a SINGLE `run_for`: `Load` + `Start` are issued before it, and a spawned
    // task injects the `Abort` a few cycles in — the occupant is polling its per-cycle
    // warm-up wait and folds the cancel at the next one.
    let captured = stellarator::run(|| async move {
        let ((run_states, modes), sampler) = spawn_sampler(seq, mode);
        let sampler = sampler.drop_guard();

        control
            .emit(&SequenceCommand {
                channel: "mode".to_string(),
                command: SequenceCommandKind::Load {
                    name: "commissioning".to_string(),
                },
            })
            .unwrap();
        control
            .emit(&SequenceCommand {
                channel: "mode".to_string(),
                command: SequenceCommandKind::Start,
            })
            .unwrap();
        let aborter = stellarator::spawn(async move {
            for _ in 0..4 {
                stellarator::yield_now().await;
            }
            control
                .emit(&SequenceCommand {
                    channel: "mode".to_string(),
                    command: SequenceCommandKind::Abort,
                })
                .unwrap();
        })
        .drop_guard();

        coord.run_for(40).await;
        drop((sampler, aborter));
        (run_states, modes)
    });
    let (run_states, modes) = captured;
    let run_states = run_states.borrow();
    let modes = modes.borrow();

    assert_eq!(
        run_states.last(),
        Some(&ABORTED),
        "the aborted occupant ended Aborted: {run_states:?}"
    );
    assert!(
        modes.contains(&ModeCmd::SAFE),
        "the safing branch emitted ModeCmd::safe: {modes:?}"
    );
    assert!(
        !modes.contains(&ModeCmd::POINTING),
        "it was aborted before pointing: {modes:?}"
    );
}
