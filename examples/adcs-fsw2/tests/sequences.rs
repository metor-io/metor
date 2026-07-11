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
//!    its condition-based ladder: estimator warm-up (no ModeCmd — ctrl holds identity, which
//!    already damps the boot tumble), then settling → pointing gated on the live tracking
//!    error (detumble is skipped — the warm-up hold leaves the rate far below the enter gate).
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
use metor_fsw_2::metor_proto::types::{ComponentId, Msg};
use metor_fsw_2::metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind, SequenceRegistry,
};
use metor_fsw_2::wiring::Registry;
use metor_fsw_2::wiring::{build_artifacts, parse, resolve};
use metor_fsw_2::{BuildOptions, Coordinator, Input, SequenceStatus, split_record};

/// The mission wiring document — the same file the CLI runner and the other tests read.
const MISSION_KDL: &str = include_str!("../mission.kdl");

// `SequenceStatus::run_state` codes (sequence/mod.rs): 0 running, then `Outcome::run_state`.
const RUNNING: u8 = 0;
const COMPLETED: u8 = 1;
const ABORTED: u8 = 2;
const FAILED: u8 = 3;

/// Build the mission off `mission.kdl`. When `auto_run` is false the slot's `initial`
/// occupant is cleared so the slot starts **empty** (the interactive scenario drives it by
/// hand); otherwise the KDL's `initial ... state="running"` stands. `None` if the build
/// plumbing is unavailable (so the caller skips rather than fails spuriously, like `bundle`).
fn build_mission(auto_run: bool) -> Option<Coordinator> {
    let mut wiring = parse(MISSION_KDL).expect("parse mission.kdl");
    // Test binaries can't host a `process=#true` worker (no `worker_entry` in main) — run
    // every system in-process, like `build_sim_coordinator`.
    for spec in &mut wiring.systems {
        spec.process = false;
    }
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
    Some(resolve(&wiring, &Registry::with_builtins()).expect("resolve the mission"))
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
            if let Some(r) = seq.latest() {
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

    // ~4000 cycles ≈ 33 s of sim time (the closed_loop budget): the ladder is
    // condition-based now — estimator warm-up (~2 s), then a real slew until the tracking
    // error holds under the coarse gate, then the fine-pointing confirm dwell.
    let (captured, _coord) = stellarator::run(|| async move {
        let ((run_states, modes), sampler) = spawn_sampler(seq, mode);
        let sampler = sampler.drop_guard();
        coord.run_for(4000).await;
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
        run_states.contains(&RUNNING),
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

// ---------------------------------------------------------------------------
// 3. Downlink (Wave 5): the real mission's `mode` slot emits the ordered
//    commissioning lifecycle as `SequenceChannelEvent`s on its message channel, and
//    the coordinator emits a boot `SequenceRegistry` listing the slot's allowed
//    occupants — the exact wire path the panel's sequence view sources. Taps the
//    message rings via `Coordinator::registry()` (docs/messages.md §5).
// ---------------------------------------------------------------------------

/// Drain a message ring into the decoded `Msg`s of one type, asserting the 2-byte id and
/// postcard-round-tripping each record — the downlink tap's decode (`docs/messages.md` §3),
/// reused verbatim from the framework's `slot_integration` test.
fn drain_msgs<M: Msg + serde::de::DeserializeOwned>(
    view: &mut metor_fsw_2::ring::View<
        metor_fsw_2::ring::NoWake,
        metor_fsw_2::ring::NoWake,
    >,
) -> Vec<M> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    while view.try_read_into(&mut buf).expect("no lap on the message tap") {
        let (id, payload) = split_record(&buf).expect("a 2-byte-id record");
        assert_eq!(id, M::ID, "every record on this channel carries M::ID");
        out.push(postcard::from_bytes::<M>(payload).expect("postcard round-trip"));
    }
    out
}

#[test]
fn commissioning_emits_ordered_sequence_messages() {
    let Some(mut coord) = build_mission(true) else {
        return;
    };

    // Tap the `mode` slot's events channel + the coordinator's boot-registry channel BEFORE
    // the run — the message rings are overwrite, starting at the live edge, so the taps must
    // precede the emits. Only transition records land (6 events + 1 registry over the run),
    // far under the message ring depth, so a post-run drain never laps.
    let messages = coord.registry();
    let mut events_view = messages
        .view(ComponentId::new("mode.sequences"))
        .expect("the slot events channel is registered")
        .expect("a reader slot is available");
    let mut boot_view = messages
        .view(ComponentId::new("coordinator.sequences"))
        .expect("the coordinator boot-registry channel is registered")
        .expect("a reader slot is available");

    // The same ~4000-cycle auto-run window the `commissioning_auto_runs_to_completion` test
    // uses — enough for the condition-based ladder to walk to `Completed`.
    let coord = stellarator::run(|| async move {
        coord.run_for(4000).await;
        coord
    });

    // (a) The boot `SequenceRegistry` lists the `mode` channel with both allowed occupants.
    let registries = drain_msgs::<SequenceRegistry>(&mut boot_view);
    let registry = registries.last().expect("a boot SequenceRegistry was emitted");
    let channel = registry
        .channels
        .iter()
        .find(|c| c.name == "mode")
        .expect("the registry lists the `mode` slot channel");
    assert!(
        channel.available.contains(&"commissioning".to_string())
            && channel.available.contains(&"safe_mode".to_string()),
        "the `mode` channel advertises both allowed occupants: {:?}",
        channel.available
    );

    // (b) The `mode` slot's events, IN ORDER with no coalescing: the full commissioning
    //     lifecycle. The progress detail strings match `systems/commissioning/src/lib.rs`.
    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    assert!(
        events.iter().all(|e| e.channel == channel.name),
        "every event is tagged with the `mode` channel's name"
    );
    let kinds: Vec<&SequenceEventKind> = events.iter().map(|e| &e.kind).collect();
    // The full lifecycle, every-record (no coalescing). The progress detail strings match
    // `systems/commissioning/src/lib.rs`, enumerated once here.
    let expected_progress = ["estimator warm-up", "coarse pointing", "pointing", "commissioned"];
    assert_eq!(
        kinds.len(),
        3 + expected_progress.len(),
        "Loaded + Started + progress lines + Completed: {kinds:?}"
    );
    assert!(
        matches!(kinds[0], SequenceEventKind::Loaded { name } if name == "commissioning"),
        "[0] Loaded {{ commissioning }}: {kinds:?}"
    );
    assert!(matches!(kinds[1], SequenceEventKind::Started), "[1] Started: {kinds:?}");
    for (i, expected) in expected_progress.iter().enumerate() {
        assert!(
            matches!(kinds[2 + i], SequenceEventKind::Progress { detail } if detail == expected),
            "[{}] Progress {{ {expected} }}: {kinds:?}",
            2 + i
        );
    }
    assert!(
        matches!(kinds.last().unwrap(), SequenceEventKind::Completed),
        "[last] Completed: {kinds:?}"
    );

    drop((coord, events_view, boot_view, messages));
}

// ---------------------------------------------------------------------------
// 4. Detumble: entered when the estimated rate exceeds the (patched) gate, the
//    wheels idle under LAW_DETUMBLE, and a phase timeout safes + Fails. The
//    mission gate (1.0 rad/s) is deliberately never reached by the boot tumble,
//    so this scenario patches the allow-line params — and uses the timeout path
//    as its deterministic assertion (magnetorquer authority is far too small to
//    finish a real detumble inside a test budget).
// ---------------------------------------------------------------------------

#[test]
fn detumble_times_out_to_failed_with_wheels_idle() {
    // Enter far below the ~0.1 rad/s the warm-up identity-hold leaves; time out after 2 s
    // of sim (B-cross barely dents the rate in that window).
    let kdl = MISSION_KDL
        .replace("rate_detumble_enter=1.0", "rate_detumble_enter=0.01")
        .replace("rate_detumble_exit=0.8", "rate_detumble_exit=0.005")
        .replace("detumble_timeout_s=900.0", "detumble_timeout_s=2.0");
    assert!(kdl != MISSION_KDL, "patch anchors present in mission.kdl");
    let mut wiring = parse(&kdl).expect("parse the patched mission.kdl");
    for spec in &mut wiring.systems {
        spec.process = false;
    }
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    let mut coord =
        resolve(&wiring, &Registry::with_builtins()).expect("resolve the patched mission");
    let (seq, mode) = tap_slot(&mut coord);
    // A second mode view + the controller's torque output, for the wheels-idle assertion.
    let (_, mode_for_torque) = tap_slot(&mut coord);
    let torque_view: Input<adcs_contracts::TorqueCmd> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("ctrl.torque_cmd"))
            .expect("ctrl.torque_cmd is registered")
            .expect("a reader slot is available"),
    );

    // Estimator warm-up (~5 s — the identity-hold must damp the boot tumble before the
    // q̂-delta gate settles) + the 2 s detumble timeout + margin.
    let captured = stellarator::run(|| async move {
        let ((run_states, modes), sampler) = spawn_sampler(seq, mode);
        let sampler = sampler.drop_guard();

        // While the commanded mode is DETUMBLE (with a few cycles of grace for the delayed
        // mode_cmd edge into ctrl), the wheels must idle: ctrl publishes an exactly-zero
        // torque under LAW_DETUMBLE.
        let max_detumble_torque = Rc::new(RefCell::new(0.0f64));
        let captured_torque = max_detumble_torque.clone();
        let torque_sampler = stellarator::spawn(async move {
            let (mut mode, mut torque) = (mode_for_torque, torque_view);
            let mut current_mode = ModeCmd::IDLE;
            let mut cycles_in_mode = 0u32;
            loop {
                stellarator::yield_now().await;
                if let Some(m) = mode.latest() {
                    let m = m.get().mode;
                    if m == current_mode {
                        cycles_in_mode += 1;
                    } else {
                        current_mode = m;
                        cycles_in_mode = 0;
                    }
                }
                let record = current_mode == ModeCmd::DETUMBLE && cycles_in_mode >= 3;
                let _ = torque.drain(|f| {
                    if record {
                        let mag: f64 = f.get().torque_b.norm().into_buf();
                        let mut max = captured_torque.borrow_mut();
                        *max = max.max(mag);
                    }
                });
            }
        })
        .drop_guard();

        coord.run_for(1500).await;
        drop((sampler, torque_sampler));
        (run_states, modes, max_detumble_torque)
    });
    let (run_states, modes, max_detumble_torque) = captured;
    let run_states = run_states.borrow();
    let modes = modes.borrow();

    assert_eq!(
        run_states.last(),
        Some(&FAILED),
        "detumble timed out to Failed: modes {modes:?}, last run_states {:?}",
        &run_states[run_states.len().saturating_sub(5)..]
    );
    assert_eq!(
        &*modes,
        &[ModeCmd::DETUMBLE, ModeCmd::SAFE],
        "detumble entered, then the timeout safed: {modes:?}"
    );
    let max_torque = *max_detumble_torque.borrow();
    assert!(
        max_torque == 0.0,
        "the wheels idled while detumbling: max |torque| = {max_torque:e}"
    );
}
