//! Integration tests for runtime slots driving a real `dlopen`ed sequence,
//! constructed through the [`Wiring`] front end.
//!
//! Each test builds the `metor-fsw-2-seq-fixture` crate as a `cdylib`, declares
//! a `slot` target with [`WiringBuilder`], points the artifact at the located
//! `.so`, and [`resolve`]s it into a [`Coordinator`]. The `waiter`/`napper`
//! sequence occupants and the `beater` cyclic occupant are all pack entries the
//! fixture exports, allowed into a slot by name. The tests then drive the
//! slot's lifecycle with [`SequenceCommand`]s and observe it from the outside.
//! Together they cover the command-to-phase state machine, occupant swap, name
//! addressing, the boot registry, and the build-time rejections a bad slot
//! wiring surfaces as a [`LoadError`](metor_fsw_2::wiring::LoadError).
//!
//! Two conventions run through every test:
//!
//! * Commands are ordinary dataflow. A slot acts only on [`SequenceCommand`]s
//!   that arrive over an explicitly connected edge, so each target wires its
//!   producer (the in-process control handle or another system) to the slot
//!   with an explicit message edge.
//! * Status taps are opened before the run. An overwrite ring's reader starts
//!   at the live edge, so a view created after the run would see nothing.
//!
//! The scripted-uplink command-dispatch tests, which need a test-double
//! transport the `Wiring` path cannot express, live in-crate under
//! `coordinator::uplink_tests`. The fixture build runs inside the test; if the
//! build plumbing is unavailable the body is skipped rather than failed.

#![cfg(all(feature = "wiring", not(miri)))]

use std::path::{Path, PathBuf};

mod common;

use metor_fsw_2::metor_proto::types::{ComponentId, Msg};
use metor_fsw_2::metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind, SequenceRegistry,
};
use metor_fsw_2::wiring::{LoadErrorKind, Registry, resolve};
use metor_fsw_2::{
    AllowedOccupantSpec, BuildSystem, ClockSpec, CommandOut, Coordinator, CoordinatorSpec,
    CyclicSystem, InitialOccupantSpec, Input, NAME_CAP, Out, ParamSource, SequenceStatus,
    SlotInitState, SlotSpec, SlotStatus, System, SystemInput, SystemOutput, Timestamp, WireError,
    Wiring, WiringBuilder, split_record,
};

/// The seq fixture's cargo crate name and cdylib library stem.
const FIXTURE_CRATE: &str = "metor-fsw-2-seq-fixture";
const FIXTURE_STEM: &str = "metor_fsw_2_seq_fixture";

/// A `Load { occupant }` command addressed to the channel named `ch`.
fn load(ch: &str, occupant: &str) -> SequenceCommand {
    SequenceCommand {
        channel: ch.to_string(),
        command: SequenceCommandKind::Load {
            name: occupant.to_string(),
        },
    }
}

/// Any other command addressed to the channel named `ch`.
fn cmd(ch: &str, command: SequenceCommandKind) -> SequenceCommand {
    SequenceCommand {
        channel: ch.to_string(),
        command,
    }
}

// ---------------------------------------------------------------------------
// Building and locating the fixture cdylib
// ---------------------------------------------------------------------------

/// Build the seq fixture cdylib and locate it, skipping on failure.
fn locate_fixture() -> Option<PathBuf> {
    common::locate_fixture(FIXTURE_CRATE, FIXTURE_STEM)
}

/// A 1000 Hz, depth-8 simulated target whose 2µs-per-cycle clock elapses the
/// fixture's 2µs wait in a single step — the shared coordinator config.
fn seq_coordinator() -> CoordinatorSpec {
    CoordinatorSpec {
        cycle_rate: 1000.0,
        default_depth: Some(8),
        clock: ClockSpec::Simulated { dt_secs: 0.000_002 },
        namespace: None,
    }
}

/// Point the target's single artifact at the located fixture (in place of the
/// build driver) and resolve it through the empty registry into a coordinator.
fn resolve_slot(mut wiring: Wiring, lib: &Path) -> Coordinator {
    wiring.artifacts[0].path = Some(lib.to_path_buf());
    resolve(&wiring, &Registry::new()).expect("resolve the slot Wiring")
}

/// Drain every `SlotStatus.phase` published over a run.
fn slot_phases(view: &mut Input<SlotStatus>) -> Vec<u8> {
    let mut phases = Vec::new();
    view.drain(|f| phases.push(f.get().phase)).expect("no lap");
    phases
}

/// Drain every `SequenceStatus.run_state` the occupant published.
fn seq_run_states(view: &mut Input<SequenceStatus>) -> Vec<u8> {
    let mut states = Vec::new();
    view.drain(|f| states.push(f.get().run_state))
        .expect("no lap");
    states
}

// SlotState wire codes: Empty=0, Loaded=1, Loading=2, Running=3, Done=4, Stopped=5.
const LOADED: u8 = 1;
const RUNNING: u8 = 3;
const DONE: u8 = 4;
// Terminal run_state codes: Completed=1, Aborted=2.
const COMPLETED: u8 = 1;
const ABORTED: u8 = 2;

// ---------------------------------------------------------------------------
// Slot lifecycle
// ---------------------------------------------------------------------------

/// A single-`waiter` slot target with the coordinator's command edge wired to
/// it, the shared shape behind the lifecycle tests.
fn waiter_slot() -> Wiring {
    WiringBuilder::new()
        .coordinator_spec(seq_coordinator())
        .artifact("seqs", FIXTURE_CRATE, FIXTURE_STEM)
        .slot("adcs")
        .allow("waiter")
        .end()
        .connect_msg("coordinator", "adcs", "SequenceCommand")
        .build()
}

/// Load then Start runs the occupant to a terminal Done with a Completed
/// run state.
#[test]
fn slot_load_start_runs_to_done() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(waiter_slot(), &lib);

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );
    let mut seq_view: Input<SequenceStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.sequence"))
            .expect("occupant SequenceStatus is registered")
            .expect("reader slot available"),
    );

    // Load + Start are queued before the run and drained at cycle 0, addressed
    // by the slot's instance name.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();

    let coord = stellarator::run(|| async move {
        coord.run_for(4).await;
        coord
    });

    let phases = slot_phases(&mut slot_view);
    assert!(phases.contains(&RUNNING), "the slot ran: {phases:?}");
    assert_eq!(
        phases.last(),
        Some(&DONE),
        "the slot finished Done: {phases:?}"
    );

    let states = seq_run_states(&mut seq_view);
    assert_eq!(
        states.last(),
        Some(&COMPLETED),
        "the occupant reached Completed: {states:?}"
    );

    // Done is a terminal success, not an error stop.
    assert!(coord.stopped().is_empty(), "Done is not a hard-stop");

    drop((coord, slot_view, seq_view, control));
}

/// Abort is a cooperative cancel delivered as ring data; the occupant folds it
/// at its next wait and finishes Done with an Aborted run state.
#[test]
fn slot_abort_completes_aborted() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(waiter_slot(), &lib);

    let mut seq_view: Input<SequenceStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.sequence"))
            .expect("occupant SequenceStatus is registered")
            .expect("reader slot available"),
    );
    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );

    // All three commands drain at cycle 0, before the wait deadline, so the
    // very first poll already observes the cancel and takes the safing branch.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Abort))
        .unwrap();

    let coord = stellarator::run(|| async move {
        coord.run_for(3).await;
        coord
    });

    let states = seq_run_states(&mut seq_view);
    assert_eq!(
        states.last(),
        Some(&ABORTED),
        "the occupant cooperatively aborted: {states:?}"
    );
    let phases = slot_phases(&mut slot_view);
    assert_eq!(
        phases.last(),
        Some(&DONE),
        "Aborted is still terminal Done: {phases:?}"
    );

    drop((coord, seq_view, slot_view, control));
}

/// Stop drops the occupant's future outright, releasing its ring roles; the
/// slot returns to Loaded and the occupant never polls or publishes.
#[test]
fn slot_stop_hard_drops_occupant() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(waiter_slot(), &lib);

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );
    let mut seq_view: Input<SequenceStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.sequence"))
            .expect("occupant SequenceStatus is registered")
            .expect("reader slot available"),
    );

    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Stop))
        .unwrap();

    let coord = stellarator::run(|| async move {
        coord.run_for(3).await;
        coord
    });

    let phases = slot_phases(&mut slot_view);
    assert_eq!(
        phases.last(),
        Some(&LOADED),
        "after a hard-drop Stop the slot is Loaded (no live future): {phases:?}"
    );
    // The occupant was destroyed before it ever polled.
    assert!(
        seq_run_states(&mut seq_view).is_empty(),
        "a hard-dropped occupant never executed"
    );
    assert!(
        coord.stopped().is_empty(),
        "a hard-drop is not an error-stop"
    );

    drop((coord, slot_view, seq_view, control));
}

/// After a terminal Done, Reset rebuilds the occupant over the same rings and a
/// fresh Start runs it to completion again.
#[test]
fn slot_reset_reruns_from_start() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(waiter_slot(), &lib);

    let mut seq_view: Input<SequenceStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.sequence"))
            .expect("occupant SequenceStatus is registered")
            .expect("reader slot available"),
    );

    // A coordinator drives exactly one bounded run, so the second command pair
    // is injected mid-run. Cycles 1-3 take Load + Start to Completed. A spawned
    // task, polled at the simulated loop's per-cycle yield, waits for cycle 3
    // and then emits Reset + Start, which the slot drains at the head of cycle
    // 4; cycles 4-6 reload the occupant and complete again.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();
    let progress = coord.progress();
    let coord = stellarator::run(|| async move {
        let injector = stellarator::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            while progress.load(Relaxed) < 3 {
                stellarator::yield_now().await;
            }
            control
                .emit(&cmd("adcs", SequenceCommandKind::Reset))
                .unwrap();
            control
                .emit(&cmd("adcs", SequenceCommandKind::Start))
                .unwrap();
        });
        coord.run_for(6).await;
        let _ = injector.await;
        coord
    });

    let states = seq_run_states(&mut seq_view);
    let completed = states.iter().filter(|&&s| s == COMPLETED).count();
    assert!(
        completed >= 2,
        "the occupant completed once per run across the Reset reload: {states:?}"
    );

    drop((coord, seq_view));
}

/// The last published occupant name on the slot-status frame.
fn last_occupant(view: &mut Input<SlotStatus>) -> Option<String> {
    let mut last = None;
    view.drain(|f| {
        let s = f.get();
        last = Some(s.occupant.as_str().to_string());
    })
    .expect("no lap");
    last
}

/// A slot allowing two distinct occupant entries (`waiter` and `napper`, the
/// same fixture body under two names) plus the coordinator command edge.
fn two_occupant_slot() -> Wiring {
    WiringBuilder::new()
        .coordinator_spec(seq_coordinator())
        .artifact("seqs", FIXTURE_CRATE, FIXTURE_STEM)
        .slot("adcs")
        .allow("waiter")
        .allow("napper")
        .end()
        .connect_msg("coordinator", "adcs", "SequenceCommand")
        .build()
}

/// Loading over a `Loaded` slot swaps occupants: the current occupant is
/// dropped and the named one built, no Stop/Reset dance required. Two allow
/// entries over distinct fixture entries give the slot two occupant names.
#[test]
fn load_from_loaded_swaps_occupant() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(two_occupant_slot(), &lib);

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );
    let mut events_view = coord
        .registry()
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");

    // All three drain in one cycle: Load lands Loaded{waiter}, the second Load
    // swaps to Loaded{napper}, Start runs the swapped-in occupant.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control.emit(&load("adcs", "napper")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();

    let coord = stellarator::run(|| async move {
        coord.run_for(4).await;
        coord
    });

    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    let kinds: Vec<&SequenceEventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        matches!(kinds[0], SequenceEventKind::Loaded { name } if name == "waiter"),
        "first event is Loaded {{ waiter }}: {kinds:?}"
    );
    assert!(
        matches!(kinds[1], SequenceEventKind::Loaded { name } if name == "napper"),
        "the swap re-Loads in place: {kinds:?}"
    );
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, SequenceEventKind::Refused { .. })),
        "nothing was refused: {kinds:?}"
    );
    assert!(
        matches!(kinds.last().unwrap(), SequenceEventKind::Completed),
        "the swapped-in occupant ran to Completed: {kinds:?}"
    );
    assert_eq!(
        last_occupant(&mut slot_view).as_deref(),
        Some("napper"),
        "the slot ends on the swapped-in occupant"
    );

    drop((coord, slot_view, events_view, control));
}

/// The reported wedge: run to Done, Reset (slot returns to Loaded), then Load
/// a different occupant. Before the fix the Load was silently refused and the
/// channel was stuck until another terminal state.
#[test]
fn reset_then_load_swaps_occupant() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(two_occupant_slot(), &lib);

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );
    let mut events_view = coord
        .registry()
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");

    // Cycles 1-3 run "waiter" to Done. The injector then replays the panel
    // flow that used to wedge: Reset (Done -> Loaded), Load{napper}, Start.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();
    let progress = coord.progress();
    let coord = stellarator::run(|| async move {
        let injector = stellarator::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            while progress.load(Relaxed) < 3 {
                stellarator::yield_now().await;
            }
            control
                .emit(&cmd("adcs", SequenceCommandKind::Reset))
                .unwrap();
            control.emit(&load("adcs", "napper")).unwrap();
            control
                .emit(&cmd("adcs", SequenceCommandKind::Start))
                .unwrap();
        });
        coord.run_for(7).await;
        let _ = injector.await;
        coord
    });

    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    let kinds: Vec<&SequenceEventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, SequenceEventKind::Refused { .. })),
        "the Reset -> Load flow refuses nothing: {kinds:?}"
    );
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, SequenceEventKind::Loaded { name } if name == "napper")),
        "the post-Reset Load swapped occupants: {kinds:?}"
    );
    let completed = kinds
        .iter()
        .filter(|k| matches!(k, SequenceEventKind::Completed))
        .count();
    assert_eq!(completed, 2, "both occupants ran to Completed: {kinds:?}");
    assert_eq!(
        last_occupant(&mut slot_view).as_deref(),
        Some("napper"),
        "the slot ends on the swapped-in occupant"
    );

    drop((coord, slot_view, events_view));
}

/// A `Load` while the occupant is running is refused — loudly, with a
/// `Refused` event — and changes nothing.
#[test]
fn load_while_running_is_refused() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(two_occupant_slot(), &lib);

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );
    let mut events_view = coord
        .registry()
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");

    // One drain applies all three: Load{waiter} -> Loaded, Start -> Running,
    // Load{napper} -> refused (the occupant is live).
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();
    control.emit(&load("adcs", "napper")).unwrap();

    let coord = stellarator::run(|| async move {
        coord.run_for(4).await;
        coord
    });

    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    let kinds: Vec<&SequenceEventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            SequenceEventKind::Refused { reason } if reason.contains("running")
        )),
        "the mid-run Load was refused with a reason: {kinds:?}"
    );
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, SequenceEventKind::Loaded { name } if name == "napper")),
        "the refused Load loaded nothing: {kinds:?}"
    );
    assert_eq!(
        last_occupant(&mut slot_view).as_deref(),
        Some("waiter"),
        "the running occupant kept the slot"
    );

    drop((coord, slot_view, events_view, control));
}

// ---------------------------------------------------------------------------
// Sequence events and the boot registry
// ---------------------------------------------------------------------------

/// Drain a message ring, decoding every record as `M` after checking its
/// 2-byte id.
fn drain_msgs<M: Msg + serde::de::DeserializeOwned>(
    view: &mut metor_fsw_2::ring::View<metor_fsw_2::ring::NoWake>,
) -> Vec<M> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    while view
        .try_read_into(&mut buf)
        .expect("no lap on the message tap")
    {
        let (id, payload) = split_record(&buf).expect("a 2-byte-id record");
        assert_eq!(id, M::ID, "every record on this channel carries M::ID");
        out.push(postcard::from_bytes::<M>(payload).expect("postcard round-trip"));
    }
    out
}

/// The slot's message channel publishes ordered [`SequenceChannelEvent`]s with
/// no coalescing, and the coordinator publishes a boot [`SequenceRegistry`]
/// listing each slot with its allowed occupants.
#[test]
fn slot_emits_ordered_sequence_events_and_boot_registry() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(waiter_slot(), &lib);

    let messages = coord.registry();
    let mut events_view = messages
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");
    let mut boot_view = messages
        .view(ComponentId::new("coordinator.sequences"))
        .expect("the coordinator boot-registry channel is registered")
        .expect("reader slot available");

    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();
    let coord = stellarator::run(|| async move {
        coord.run_for(4).await;
        coord
    });

    let registries = drain_msgs::<SequenceRegistry>(&mut boot_view);
    let registry = registries
        .last()
        .expect("a boot SequenceRegistry was emitted");
    assert_eq!(registry.channels.len(), 1, "one slot channel");
    assert_eq!(registry.channels[0].name, "adcs");
    assert_eq!(registry.channels[0].available, vec!["waiter".to_string()]);

    // The full event order is Loaded, Started, the progress lines, Completed;
    // the fixture emits "waiting" then "done" as its progress lines.
    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    assert!(
        events.iter().all(|e| e.channel == "adcs"),
        "every event is tagged with the slot's instance name"
    );
    let kinds: Vec<&SequenceEventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        matches!(kinds[0], SequenceEventKind::Loaded { name } if name == "waiter"),
        "first event is Loaded {{ waiter }}: {kinds:?}"
    );
    assert!(
        matches!(kinds[1], SequenceEventKind::Started),
        "second event is Started: {kinds:?}"
    );
    assert!(
        matches!(kinds.last().unwrap(), SequenceEventKind::Completed),
        "last event is Completed: {kinds:?}"
    );
    let waiting = kinds
        .iter()
        .position(|k| matches!(k, SequenceEventKind::Progress { detail } if detail == "waiting"))
        .expect("a Progress{waiting} event");
    let done = kinds
        .iter()
        .position(|k| matches!(k, SequenceEventKind::Progress { detail } if detail == "done"))
        .expect("a Progress{done} event");
    assert!(
        1 < waiting && waiting < done && done < kinds.len() - 1,
        "Progress ordered: {kinds:?}"
    );

    drop((coord, events_view, boot_view, control));
}

// ---------------------------------------------------------------------------
// Name addressing
// ---------------------------------------------------------------------------

/// The slot's instance name is the wire address, so an over-long name would
/// telemeter truncated while addressing untruncated; the build rejects it. The
/// resolve path surfaces the build-time [`WireError`] wrapped in a
/// [`LoadErrorKind::Wire`], carrying the precise variant.
#[test]
fn slot_name_over_the_cap_is_a_build_error() {
    let Some(lib) = locate_fixture() else {
        return;
    };

    let long = "a".repeat(NAME_CAP + 1);
    let mut wiring = WiringBuilder::new()
        .coordinator_spec(seq_coordinator())
        .artifact("seqs", FIXTURE_CRATE, FIXTURE_STEM)
        .slot(long.clone())
        .allow("waiter")
        .end()
        .build();
    wiring.artifacts[0].path = Some(lib);
    let err = resolve(&wiring, &Registry::new())
        .err()
        .expect("an over-cap slot name fails the build");
    match err.kind {
        LoadErrorKind::Wire {
            source: WireError::SlotNameTooLong { name, len },
        } => {
            assert_eq!(name, long);
            assert_eq!(len, NAME_CAP + 1);
        }
        other => panic!("expected wrapped SlotNameTooLong, got {other:?}"),
    }
}

/// A command whose channel names no slot is dropped by every slot's filter,
/// with no panic, no state change, and no event.
#[test]
fn misaddressed_command_matches_no_slot() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(waiter_slot(), &lib);

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );
    let messages = coord.registry();
    let mut events_view = messages
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");

    // A command for another channel and one whose name exceeds the cap; both
    // simply match no slot.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("other", "waiter")).unwrap();
    control.emit(&load(&"x".repeat(64), "waiter")).unwrap();
    control
        .emit(&cmd("other", SequenceCommandKind::Start))
        .unwrap();

    let coord = stellarator::run(|| async move {
        coord.run_for(3).await;
        coord
    });

    let phases = slot_phases(&mut slot_view);
    assert!(
        phases.iter().all(|&p| p == 0),
        "the misaddressed commands left the slot Empty: {phases:?}"
    );
    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    assert!(
        events.is_empty(),
        "no event was emitted for commands addressed elsewhere: {events:?}"
    );

    drop((coord, slot_view, events_view, control));
}

/// Build two slots in the given declaration order and command the one named
/// "adcs". Commands address slots by name, never by declaration index, so the
/// adcs slot must run either way and the recovery slot must never leave Empty.
/// One order per `#[test]` because `stellarator::run` is once per thread.
fn drive_adcs_of_two_slots(adcs_first: bool) {
    let Some(lib) = locate_fixture() else {
        return;
    };

    // The one coordinator producer is edged to both slots; only the slot whose
    // name matches a command's channel acts on it, so broad fan-out is
    // harmless. Declaration order is the variable under test.
    let mut builder = WiringBuilder::new()
        .coordinator_spec(seq_coordinator())
        .artifact("seqs", FIXTURE_CRATE, FIXTURE_STEM);
    for name in if adcs_first {
        ["adcs", "recovery"]
    } else {
        ["recovery", "adcs"]
    } {
        builder = builder.slot(name).allow("waiter").end().connect_msg(
            "coordinator",
            name,
            "SequenceCommand",
        );
    }
    let mut coord = resolve_slot(builder.build(), &lib);

    let mut adcs_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("adcs slot status is registered")
            .expect("reader slot available"),
    );
    let mut recovery_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("recovery.slot_status"))
            .expect("recovery slot status is registered")
            .expect("reader slot available"),
    );

    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();

    let coord = stellarator::run(|| async move {
        coord.run_for(4).await;
        coord
    });

    let phases = slot_phases(&mut adcs_view);
    assert!(
        phases.contains(&RUNNING),
        "adcs ran (declared {} first): {phases:?}",
        if adcs_first { "adcs" } else { "recovery" }
    );
    assert_eq!(phases.last(), Some(&DONE), "adcs finished Done: {phases:?}");
    let recovery = slot_phases(&mut recovery_view);
    assert!(
        recovery.iter().all(|&p| p == 0),
        "the recovery slot never left Empty: {recovery:?}"
    );

    drop((coord, adcs_view, recovery_view, control));
}

#[test]
fn command_addresses_slot_by_name_adcs_declared_first() {
    drive_adcs_of_two_slots(true);
}

#[test]
fn reordering_slots_does_not_readdress_commands() {
    drive_adcs_of_two_slots(false);
}

// ---------------------------------------------------------------------------
// Command edges are explicit dataflow
// ---------------------------------------------------------------------------

/// A stand-in autonomy emitter that sends `Load { waiter }` and `Start` to the
/// `adcs` channel on its first cycle.
struct Autonomy {
    sent: bool,
}

#[derive(SystemInput)]
struct AutonomyIn {}

#[derive(SystemOutput)]
struct AutonomyOut {
    commands: CommandOut<SequenceCommand>,
}

impl System for Autonomy {
    type Input = AutonomyIn;
    type Output = Out<AutonomyOut>;
    const NAME: &'static str = "autonomy";
}

impl CyclicSystem for Autonomy {
    fn execute(&mut self, _now: Timestamp, _in: &mut AutonomyIn, o: &mut Self::Output) {
        if !self.sent {
            self.sent = true;
            let _ = o.commands.emit(&load("adcs", "waiter"));
            let _ = o.commands.emit(&cmd("adcs", SequenceCommandKind::Start));
        }
    }
}

impl BuildSystem for Autonomy {
    type Params = ();
    fn new(_params: ()) -> Self {
        Autonomy { sent: false }
    }
}

/// Build one slot plus the autonomy emitter, with or without the command edge,
/// and return the phases the slot published over a short run.
fn autonomy_phases(edged: bool) -> Option<Vec<u8>> {
    let lib = locate_fixture()?;

    let mut builder = WiringBuilder::new()
        .coordinator_spec(seq_coordinator())
        .artifact("seqs", FIXTURE_CRATE, FIXTURE_STEM)
        .system("autonomy")
        .ty("Autonomy")
        .end()
        .slot("adcs")
        .allow("waiter")
        .end();
    if edged {
        builder = builder.connect_msg("autonomy", "adcs", "SequenceCommand");
    }
    let mut wiring = builder.build();
    wiring.artifacts[0].path = Some(lib);

    let mut registry = Registry::new();
    registry.register::<Autonomy, _>("Autonomy");
    let mut coord = resolve(&wiring, &registry).expect("the autonomy + slot graph resolves");

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );
    let coord = stellarator::run(|| async move {
        coord.run_for(5).await;
        coord
    });
    let phases = slot_phases(&mut slot_view);
    drop((coord, slot_view));
    Some(phases)
}

/// Declaring a command output is not enough; without a connected edge the
/// emitter's commands reach nothing.
#[test]
fn command_output_without_an_edge_commands_nothing() {
    let Some(phases) = autonomy_phases(false) else {
        return;
    };
    assert!(
        phases.iter().all(|&p| p == 0),
        "an un-edged SequenceCommand producer drives no slot: {phases:?}"
    );
}

/// The same emit, now over a declared edge, drives the slot to completion.
#[test]
fn command_output_with_an_edge_drives_the_slot() {
    let Some(phases) = autonomy_phases(true) else {
        return;
    };
    assert!(
        phases.contains(&RUNNING),
        "the edged emitter drove the slot: {phases:?}"
    );
    assert_eq!(phases.last(), Some(&DONE), "and it ran to Done: {phases:?}");
}

// ---------------------------------------------------------------------------
// Build-time slot wiring rejections
// ---------------------------------------------------------------------------

/// An edge into a slot's runner-held self-tap input is rejected. Slot a's
/// occupant-bound `SequenceStatus` output shares its id with slot b's
/// `SequenceStatus` self-tap input, the only edge-addressable non-Edge input,
/// so resolve's `connect` (which checks only ids) succeeds and the build fails
/// as a [`WireError::HostPort`] — surfaced here wrapped in a
/// [`LoadErrorKind::Wire`] — rather than binding a foreign producer into a
/// runner-held view.
#[test]
fn edge_into_a_host_connected_input_is_rejected() {
    let Some(lib) = locate_fixture() else {
        return;
    };

    let mut wiring = WiringBuilder::new()
        .coordinator_spec(seq_coordinator())
        .artifact("seqs", FIXTURE_CRATE, FIXTURE_STEM)
        .slot("a")
        .allow("waiter")
        .end()
        .slot("b")
        .allow("waiter")
        .end()
        .connect("a", "sequence", "b", "sequence")
        .build();
    wiring.artifacts[0].path = Some(lib);
    let err = resolve(&wiring, &Registry::new())
        .err()
        .expect("an edge into a self-tap input fails");
    match err.kind {
        LoadErrorKind::Wire {
            source: WireError::HostPort { system, .. },
        } => assert_eq!(system, "b"),
        other => panic!("expected wrapped HostPort, got {other:?}"),
    }
}

/// An initial occupant outside the allowed set is rejected at resolve, before
/// any artifact is opened, as an [`LoadErrorKind::UnknownInitialOccupant`] (the
/// front-end mapping of the builder's `SlotConfigError::UnknownInitial`, which
/// `coordinator::tests::add_slot_rejects_contract_violations` pins directly).
#[test]
fn initial_occupant_outside_allowed_set_is_rejected() {
    let Some(lib) = locate_fixture() else {
        return;
    };

    // The builder's `slot(..).end()` would panic on an `initial` outside the
    // allow set; `add_slot_spec` trusts the spec, so the invalid slot reaches
    // resolve, where `validate` raises it.
    let mut wiring = WiringBuilder::new()
        .coordinator_spec(seq_coordinator())
        .artifact("seqs", FIXTURE_CRATE, FIXTURE_STEM)
        .add_slot_spec(SlotSpec {
            name: "adcs".into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            allow: vec![AllowedOccupantSpec {
                occupant: "waiter".into(),
                artifact: None,
                params: ParamSource::None,
                src: None,
            }],
            initial: Some(InitialOccupantSpec {
                occupant: "nonesuch".into(),
                state: SlotInitState::Loaded,
            }),
            process: false,
            src: None,
            scope: None,
        })
        .build();
    wiring.artifacts[0].path = Some(lib);
    let err = resolve(&wiring, &Registry::new())
        .err()
        .expect("an initial occupant outside the allowed set is rejected");
    assert!(
        matches!(err.kind, LoadErrorKind::UnknownInitialOccupant { .. }),
        "got {:?}",
        err.kind
    );
}

// ---------------------------------------------------------------------------
// A plain cyclic entry as a slot occupant
// ---------------------------------------------------------------------------

/// A single-`beater` slot target: the occupant tail is a mount property, so a
/// slot loads an ordinary cyclic entry (the fixture's fn-style `beater`).
fn beater_slot() -> Wiring {
    WiringBuilder::new()
        .coordinator_spec(seq_coordinator())
        .artifact("seqs", FIXTURE_CRATE, FIXTURE_STEM)
        .slot("adcs")
        .allow("beater")
        .end()
        .connect_msg("coordinator", "adcs", "SequenceCommand")
        .build()
}

fn beater_views(coord: &Coordinator) -> (Input<SequenceStatus>, Input<SlotStatus>) {
    let seq: Input<SequenceStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.sequence"))
            .expect("the mount-appended SequenceStatus is registered")
            .expect("reader slot available"),
    );
    let slot: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );
    (seq, slot)
}

/// Loaded and started, a cyclic occupant runs steadily: run_state 0 every
/// step, never terminal on its own.
#[test]
fn cyclic_occupant_runs_steadily() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(beater_slot(), &lib);
    let (mut seq_view, mut slot_view) = beater_views(&coord);
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "beater")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();
    let coord = stellarator::run(|| async move {
        coord.run_for(5).await;
        coord
    });
    let states = seq_run_states(&mut seq_view);
    assert!(!states.is_empty(), "the occupant published status");
    assert!(
        states.iter().all(|s| *s == 0),
        "a cyclic occupant never completes by itself: {states:?}"
    );
    let phases = slot_phases(&mut slot_view);
    assert_eq!(phases.last(), Some(&RUNNING), "still running: {phases:?}");
    drop((coord, seq_view, slot_view, control));
}

/// An Abort latches: the occupant wrapper stops stepping the inner driver
/// and reports a terminal `Aborted`.
#[test]
fn cyclic_occupant_abort_is_terminal() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let mut coord = resolve_slot(beater_slot(), &lib);
    let (mut seq_view, mut slot_view) = beater_views(&coord);
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "beater")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Abort))
        .unwrap();
    let coord = stellarator::run(|| async move {
        coord.run_for(3).await;
        coord
    });
    let states = seq_run_states(&mut seq_view);
    assert_eq!(
        states.last(),
        Some(&ABORTED),
        "abort is a terminal Aborted for a cyclic occupant: {states:?}"
    );
    let phases = slot_phases(&mut slot_view);
    assert_eq!(phases.last(), Some(&DONE), "terminal Done: {phases:?}");
    drop((coord, seq_view, slot_view, control));
}
