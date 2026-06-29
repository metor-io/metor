//! The slots/sequences end-to-end gate (WP10 Wave 7): the `adcs-fsw2` mission's `mode`
//! **slot** runs a real `#[sequence]` occupant through the full `dlopen` path, alongside the
//! untouched plant/nav/ctrl loop.
//!
//! Two scenarios, both built from the SAME `mission.kdl` the CLI runner consumes (parse →
//! `build_artifacts` → `resolve` with an empty `Registry`, exactly as `closed_loop.rs` /
//! `bundle.rs` set up — the sequence cdylibs are built by `build_artifacts` like any
//! artifact):
//!
//! 1. **Auto-run** — the slot's `initial state="running"` `commissioning` occupant runs from
//!    the first cycle and reaches terminal `Completed` on its `SequenceStatus`, having walked
//!    `ModeCmd` settling → pointing (its two writes; idle is the pre-write default).
//! 2. **Interactive** — the same slot started **empty** and driven through
//!    [`Coordinator::control_handle`]: `Load` → `Start` → `Abort`; the occupant folds the
//!    cancel at its next `wait`, writes `ModeCmd::safe`, and ends `Aborted`.
//!
//! Both poll a per-cycle sampler (the `closed_loop` pattern): under the `Simulated` clock the
//! cycle yields once per step, so a spawned reader interleaves 1:1 and never laps the slot's
//! depth-bounded status/output rings. Gated off `miri` (it builds + `dlopen`s real cdylibs).

#![cfg(not(miri))]

use std::cell::RefCell;
use std::rc::Rc;

use adcs_contracts::ModeCmd;
use metor_fsw_2::metor_proto::types::ComponentId;
use metor_fsw_2::wiring::Registry;
use metor_fsw_2::{
    BuildOptions, Coordinator, Input, Output, SequenceStatus, SlotCommand, build_artifacts,
    parse, resolve,
};

/// The mission wiring document — the same file the CLI runner and the other tests read.
const MISSION_KDL: &str = include_str!("../mission.kdl");

// `SequenceStatus::run_state` codes (sequence/mod.rs): 0 running, then `Outcome::run_state`.
const RUNNING: u8 = 0;
const COMPLETED: u8 = 1;
const ABORTED: u8 = 2;

/// Build the mission off `mission.kdl`. When `auto_run` is false the slot's `initial`
/// occupant is cleared so the slot starts **empty** (the interactive scenario drives it by
/// hand); otherwise the KDL's `initial ... state="running"` stands. `None` if the build
/// plumbing is unavailable (so the caller skips rather than fails spuriously, like `bundle`).
fn build_mission(auto_run: bool) -> Option<Coordinator> {
    let mut wiring = parse(MISSION_KDL).expect("parse mission.kdl");
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return None;
    }
    if !auto_run {
        // Start the slot empty: the interactive scenario Loads/Starts it explicitly.
        for slot in &mut wiring.slots {
            slot.initial = None;
        }
    }
    Some(resolve(&wiring, &Registry::new()).expect("resolve the mission"))
}

/// Tap the slot's occupant `SequenceStatus` (`mode.sequence`) and its `ModeCmd` output
/// (`mode.mode_cmd`) from the coordinator registry.
fn tap_slot(coord: &mut Coordinator) -> (Input<SequenceStatus>, Input<ModeCmd>) {
    let seq = Input::new(
        coord
            .registry()
            .view(ComponentId::new("mode.sequence"))
            .expect("the slot's SequenceStatus is registered")
            .expect("a reader slot is available"),
    );
    let mode = Input::new(
        coord
            .registry()
            .view(ComponentId::new("mode.mode_cmd"))
            .expect("the slot's mode_cmd output is registered")
            .expect("a reader slot is available"),
    );
    (seq, mode)
}

/// A per-cycle sampler over the slot taps: the freshest `run_state` each cycle (`latest`, so
/// it never laps) and every distinct `ModeCmd.mode` published (`drain`, 1:1 with the cycle).
/// Returns `(run_states, modes)` captured over the run.
type Captured = (Rc<RefCell<Vec<u8>>>, Rc<RefCell<Vec<u8>>>);
fn spawn_sampler(seq: Input<SequenceStatus>, mode: Input<ModeCmd>) -> (Captured, stellarator::JoinHandle<()>) {
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

// ---------------------------------------------------------------------------
// 1. Auto-run: the initial `running` occupant completes on its own.
// ---------------------------------------------------------------------------

#[test]
fn commissioning_auto_runs_to_completion() {
    let Some(mut coord) = build_mission(true) else {
        return;
    };
    let (seq, mode) = tap_slot(&mut coord);

    // ~400 cycles ≈ 3.3 s of sim time — far more than the ~30 cycles commissioning needs
    // (100 ms + 150 ms of `wait` at the 1/120 s step ≈ 12 + 18 cycles).
    let (captured, _coord) = stellarator::run(|| async move {
        let ((run_states, modes), sampler) = spawn_sampler(seq, mode);
        let sampler = sampler.drop_guard();
        coord.run_for(400).await;
        drop(sampler);
        ((run_states, modes), coord)
    });
    let (run_states, modes) = captured;
    let run_states = run_states.borrow();
    let modes = modes.borrow();

    // The primary assertion: the occupant reached terminal `Completed`.
    assert_eq!(
        run_states.last(),
        Some(&COMPLETED),
        "commissioning reached terminal Completed: {run_states:?}"
    );
    assert!(
        run_states.iter().any(|&s| s == RUNNING),
        "it published running status before completing: {run_states:?}"
    );

    // And it walked its two ModeCmd writes: settling → pointing (idle is the pre-write
    // default the slot never emits a record for).
    assert_eq!(
        &*modes,
        &[ModeCmd::SETTLING, ModeCmd::POINTING],
        "ModeCmd transitioned settling -> pointing: {modes:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Interactive: Load → Start → Abort; the occupant safes and ends Aborted.
// ---------------------------------------------------------------------------

#[test]
fn interactive_load_then_abort_safes() {
    let Some(mut coord) = build_mission(false) else {
        return;
    };
    let (seq, mode) = tap_slot(&mut coord);
    let mut control: Output<SlotCommand> = coord.control_handle();

    // `run_for` re-runs the dl systems' (non-idempotent) `init` each call, so the slot is
    // driven inside a SINGLE `run_for`: `Load` + `Start` are issued before it, and a spawned
    // task injects the `Abort` a few cycles in — fewer than the 12 cycles the first
    // `wait(100ms)` needs, so the occupant is still suspended at it and folds the cancel.
    let captured = stellarator::run(|| async move {
        let ((run_states, modes), sampler) = spawn_sampler(seq, mode);
        let sampler = sampler.drop_guard();

        control.write(&SlotCommand::load("mode", "commissioning")).unwrap();
        control.write(&SlotCommand::start("mode")).unwrap();
        let aborter = stellarator::spawn(async move {
            for _ in 0..4 {
                stellarator::yield_now().await;
            }
            control.write(&SlotCommand::abort("mode")).unwrap();
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
