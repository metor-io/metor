//! Integration tests for runtime slots driving a real `dlopen`ed sequence.
//!
//! Each test builds the `metor-fsw-2-seq-fixture` crate as a `cdylib`, opens
//! the produced shared object through [`DlPack`], selects the `waiter` entry,
//! registers it as the allowed occupant of a slot in a [`Coordinator`], and
//! drives the slot's lifecycle with
//! [`SequenceCommand`]s. Together the tests cover the slot's ring topology (the
//! slot-owned control ring appended to the occupant's inputs, plus the
//! [`SlotStatus`] output), the command-to-phase state machine, name addressing,
//! uplink routing, and clean teardown across a genuine shared-object boundary.
//!
//! Two conventions run through every test:
//!
//! * Commands are ordinary dataflow. A slot acts only on [`SequenceCommand`]s
//!   that arrive over an explicitly connected edge, so each test wires its
//!   producer (the in-process control handle, an uplink, or another system) to
//!   the slot by hand.
//! * Status taps are opened before the run. An overwrite ring's reader starts
//!   at the live edge, so a view created after the run would see nothing.
//!
//! The fixture build runs inside the test. If the build plumbing is unavailable
//! the body is skipped rather than failed.

#![cfg(not(miri))]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use metor_fsw_2::metor_proto::types::{ComponentId, IntoLenPacket, Msg, OwnedPacket};
use metor_fsw_2::metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind, SequenceRegistry,
};
use metor_fsw_2::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorConfig, DlPack, DlSystem, Input, PortRef,
    RecvTransport, SequenceStatus, SlotStatus, SystemKind, TransportError, UplinkSystem,
    split_record,
};

/// One paramless allowed occupant.
fn occ(name: &str, system: DlSystem) -> AllowedOccupant {
    AllowedOccupant::dl(name, system, Vec::new())
}
use stellarator::buf::{IoBuf, Slice};

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
// Building and opening the fixture cdylib
// ---------------------------------------------------------------------------

fn fixture_lib_name() -> String {
    let stem = "metor_fsw_2_seq_fixture";
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// Build the fixture cdylib and return the shared object's path, parsed from
/// cargo's JSON artifact output. Returns `None` after printing a skip note when
/// the build plumbing is unavailable, so the caller skips instead of failing.
fn locate_fixture() -> Option<PathBuf> {
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "metor-fsw-2-seq-fixture",
            "--message-format=json",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "skipping: fixture build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let want = fixture_lib_name();
    for line in stdout.lines() {
        if !line.contains("compiler-artifact") || !line.contains(&want) {
            continue;
        }
        for tok in line.split('"') {
            if tok.ends_with(&want) {
                let path = PathBuf::from(tok);
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    eprintln!("skipping: built the fixture but could not locate {want} in cargo output");
    None
}

fn sim_config() -> CoordinatorConfig {
    CoordinatorConfig {
        cycle_rate: 1000.0,
        default_depth: 8,
        // Each simulated cycle advances 2µs, so the fixture's 2µs wait elapses
        // after a single step.
        clock: ClockMode::Simulated {
            dt: Duration::from_micros(2),
        },
        ..CoordinatorConfig::default()
    }
}

/// Open the fixture, select its `waiter` entry, and sanity-check the
/// reconstructed descriptor.
fn open_waiter(lib: &PathBuf) -> DlSystem {
    let loaded = DlPack::open(lib)
        .expect("DlPack::open the sequence .so")
        .system("waiter")
        .expect("select the waiter entry");
    let desc = loaded.descriptor();
    assert_eq!(desc.name, "waiter");
    assert_eq!(desc.kind, SystemKind::Cyclic);
    // The fixture declares no user ports, and the occupant tail is a mount
    // property (appended at Load), not descriptor content — so the entry
    // carries only the implicit health/log outputs.
    assert_eq!(desc.inputs.len(), 0, "no user inputs, no descriptor tail");
    assert_eq!(desc.outputs.len(), 2, "just the health + log tail");
    loaded
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

/// Load then Start runs the occupant to a terminal Done with a Completed
/// run state.
#[test]
fn slot_load_start_runs_to_done() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

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
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

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
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

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
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

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
        last = Some(
            core::str::from_utf8(&s.occupant[..s.occ_len as usize])
                .expect("utf-8 occupant name")
                .to_string(),
        );
    })
    .expect("no lap");
    last
}

/// Loading over a `Loaded` slot swaps occupants: the current occupant is
/// dropped and the named one built, no Stop/Reset dance required. Two allow
/// entries over the same fixture entry give the slot two distinct names.
#[test]
fn load_from_loaded_swaps_occupant() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let pack = DlPack::open(&lib).expect("DlPack::open the sequence .so");
    let first = pack.system("waiter").expect("select the waiter entry");
    let second = pack.system("waiter").expect("select the waiter entry again");

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("first", first), occ("second", second)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

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

    // All three drain in one cycle: Load lands Loaded{first}, the second Load
    // swaps to Loaded{second}, Start runs the swapped-in occupant.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "first")).unwrap();
    control.emit(&load("adcs", "second")).unwrap();
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
        matches!(kinds[0], SequenceEventKind::Loaded { name } if name == "first"),
        "first event is Loaded {{ first }}: {kinds:?}"
    );
    assert!(
        matches!(kinds[1], SequenceEventKind::Loaded { name } if name == "second"),
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
        Some("second"),
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
    let pack = DlPack::open(&lib).expect("DlPack::open the sequence .so");
    let first = pack.system("waiter").expect("select the waiter entry");
    let second = pack.system("waiter").expect("select the waiter entry again");

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("first", first), occ("second", second)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

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

    // Cycles 1-3 run "first" to Done. The injector then replays the panel
    // flow that used to wedge: Reset (Done -> Loaded), Load{second}, Start.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "first")).unwrap();
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
            control.emit(&load("adcs", "second")).unwrap();
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
            .any(|k| matches!(k, SequenceEventKind::Loaded { name } if name == "second")),
        "the post-Reset Load swapped occupants: {kinds:?}"
    );
    let completed = kinds
        .iter()
        .filter(|k| matches!(k, SequenceEventKind::Completed))
        .count();
    assert_eq!(completed, 2, "both occupants ran to Completed: {kinds:?}");
    assert_eq!(
        last_occupant(&mut slot_view).as_deref(),
        Some("second"),
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
    let pack = DlPack::open(&lib).expect("DlPack::open the sequence .so");
    let first = pack.system("waiter").expect("select the waiter entry");
    let second = pack.system("waiter").expect("select the waiter entry again");

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("first", first), occ("second", second)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

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

    // One drain applies all three: Load{first} -> Loaded, Start -> Running,
    // Load{second} -> refused (the occupant is live).
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "first")).unwrap();
    control
        .emit(&cmd("adcs", SequenceCommandKind::Start))
        .unwrap();
    control.emit(&load("adcs", "second")).unwrap();

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
            .any(|k| matches!(k, SequenceEventKind::Loaded { name } if name == "second")),
        "the refused Load loaded nothing: {kinds:?}"
    );
    assert_eq!(
        last_occupant(&mut slot_view).as_deref(),
        Some("first"),
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
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

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
    let mut coord = coord;
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
// Uplink command dispatch
// ---------------------------------------------------------------------------

/// A [`RecvTransport`] that replays a fixed script of pre-encoded wire packets,
/// then reports `Disconnected` like a dropped link. Each packet is real wire
/// bytes re-parsed, so the loopback exercises the same parse-and-route path a
/// network reader uses.
struct MockRecv {
    queue: std::collections::VecDeque<Vec<u8>>,
}

/// Encode one Msg to its framed wire bytes, stripping the 4-byte length prefix
/// that `OwnedPacket::parse` does not expect.
fn wire_msg<M: Msg + serde::Serialize>(msg: &M) -> Vec<u8> {
    let pkt = msg.into_len_packet();
    pkt.inner[4..].to_vec()
}

impl MockRecv {
    fn new(cmds: Vec<SequenceCommand>) -> Self {
        Self {
            queue: cmds.iter().map(wire_msg).collect(),
        }
    }

    /// A script of arbitrary pre-encoded packets.
    fn from_packets(packets: Vec<Vec<u8>>) -> Self {
        Self {
            queue: packets.into(),
        }
    }
}

impl RecvTransport for MockRecv {
    async fn recv(&mut self, _buf: Vec<u8>) -> Result<OwnedPacket<Slice<Vec<u8>>>, TransportError> {
        match self.queue.pop_front() {
            Some(bytes) => {
                let slice = bytes.try_slice(..).expect("non-empty packet");
                OwnedPacket::parse(slice).map_err(|e| TransportError::Io(Box::new(e)))
            }
            None => Err(TransportError::Disconnected),
        }
    }
}

/// A command received over the uplink lands in the slot's command ring at the
/// head of a cycle and dispatches that same cycle, with no off-by-one.
#[test]
fn uplink_command_loads_and_starts_same_cycle() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    // The slot starts empty; the uplink alone drives it.
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    let uplink = b.add_async(
        UplinkSystem::new(MockRecv::new(vec![
            SequenceCommand {
                channel: "adcs".to_string(),
                command: SequenceCommandKind::Load {
                    name: "waiter".to_string(),
                },
            },
            SequenceCommand {
                channel: "adcs".to_string(),
                command: SequenceCommandKind::Start,
            },
        ]))
        .with_msg::<SequenceCommand>(),
    );
    b.connect(
        PortRef::msg::<SequenceCommand>(uplink),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the uplink command edge connects");
    let mut coord = b.build().expect("the slot + uplink graph builds");

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );

    let coord = stellarator::run(|| async move {
        coord.run_for(6).await;
        coord
    });

    let phases = slot_phases(&mut slot_view);
    assert!(
        phases.contains(&RUNNING),
        "the uplink drove the slot to Running: {phases:?}"
    );
    assert_eq!(
        phases.last(),
        Some(&DONE),
        "the started occupant ran to Done: {phases:?}"
    );
    // Load and Start dispatched in the same cycle. A one-cycle-late Start would
    // have let the slot publish a bare Loaded phase.
    assert!(
        !phases.contains(&LOADED),
        "Load and Start landed the same cycle (no bare Loaded): {phases:?}"
    );

    drop((coord, slot_view));
}

/// A [`ReloadSequences`] request wired into the coordinator's #0 bundle
/// re-emits the [`SequenceRegistry`], so a consumer that missed the one-shot
/// boot message (a late-started panel) can recover the channel list.
#[test]
fn reload_request_reemits_registry() {
    use metor_fsw_2::metor_proto_wkt::ReloadSequences;

    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let _slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    let uplink = b.add_async(
        UplinkSystem::new(MockRecv::from_packets(vec![wire_msg(&ReloadSequences {})]))
            .with_msg::<ReloadSequences>(),
    );
    b.connect(
        PortRef::msg::<ReloadSequences>(uplink),
        PortRef::msg::<ReloadSequences>(b.coordinator_handle()),
    )
    .expect("the reload edge connects to the coordinator bundle");
    let mut coord = b.build().expect("the slot + uplink graph builds");

    let mut boot_view = coord
        .registry()
        .view(ComponentId::new("coordinator.sequences"))
        .expect("the coordinator registry channel is registered")
        .expect("reader slot available");

    let coord = stellarator::run(|| async move {
        coord.run_for(6).await;
        coord
    });

    let registries = drain_msgs::<SequenceRegistry>(&mut boot_view);
    assert!(
        registries.len() >= 2,
        "boot emission plus at least one reload re-emission: {} seen",
        registries.len()
    );
    for registry in &registries {
        assert_eq!(registry.channels.len(), 1);
        assert_eq!(registry.channels[0].name, "adcs");
        assert_eq!(registry.channels[0].available, vec!["waiter".to_string()]);
    }

    drop((coord, boot_view));
}

// ---------------------------------------------------------------------------
// Name addressing
// ---------------------------------------------------------------------------

/// The slot's instance name is the wire address, so an over-long name would
/// telemeter truncated while addressing untruncated; the build rejects it.
#[test]
fn slot_name_over_the_cap_is_a_build_error() {
    use metor_fsw_2::{NAME_CAP, WireError};
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let long = "a".repeat(NAME_CAP + 1);
    let mut b = Coordinator::builder(sim_config());
    let _slot = b.add_slot(long.clone(), vec![occ("waiter", loaded)], None).unwrap();
    let err = b
        .build()
        .err()
        .expect("an over-cap slot name fails the build");
    match err {
        WireError::SlotNameTooLong { name, len } => {
            assert_eq!(name, long);
            assert_eq!(len, NAME_CAP + 1);
        }
        other => panic!("expected SlotNameTooLong, got {other:?}"),
    }
}

/// A command whose channel names no slot is dropped by every slot's filter,
/// with no panic, no state change, and no event.
#[test]
fn misaddressed_command_matches_no_slot() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

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

    let mut b = Coordinator::builder(sim_config());
    let add = |b: &mut metor_fsw_2::CoordinatorBuilder, name: &str| {
        let slot = b.add_slot(name, vec![occ("waiter", open_waiter(&lib))], None).unwrap();
        // The one producer is edged to both slots; only the slot whose name
        // matches a command's channel acts on it, so broad fan-out is harmless.
        b.connect(
            PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
            PortRef::msg::<SequenceCommand>(slot),
        )
        .expect("the coordinator command edge connects");
    };
    if adcs_first {
        add(&mut b, "adcs");
        add(&mut b, "recovery");
    } else {
        add(&mut b, "recovery");
        add(&mut b, "adcs");
    }
    let mut coord = b.build().expect("the two-slot graph builds");

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

#[derive(metor_fsw_2::SystemInput)]
struct AutonomyIn {}

#[derive(metor_fsw_2::SystemOutput)]
struct AutonomyOut {
    commands: metor_fsw_2::CommandOut<SequenceCommand>,
}

impl metor_fsw_2::System for Autonomy {
    type Input = AutonomyIn;
    type Output = metor_fsw_2::Out<AutonomyOut>;
    const NAME: &'static str = "autonomy";
}

impl metor_fsw_2::CyclicSystem for Autonomy {
    fn execute(
        &mut self,
        _now: metor_fsw_2::Timestamp,
        _in: &mut AutonomyIn,
        o: &mut Self::Output,
    ) {
        if !self.sent {
            self.sent = true;
            let _ = o.commands.emit(&load("adcs", "waiter"));
            let _ = o.commands.emit(&cmd("adcs", SequenceCommandKind::Start));
        }
    }
}

/// Build one slot plus the autonomy emitter, with or without the command edge,
/// and return the phases the slot published over a short run.
fn autonomy_phases(edged: bool) -> Option<Vec<u8>> {
    let lib = locate_fixture()?;
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let autonomy = b.add_cyclic(Autonomy { sent: false });
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    if edged {
        b.connect(
            PortRef::msg::<SequenceCommand>(autonomy),
            PortRef::msg::<SequenceCommand>(slot),
        )
        .expect("the autonomy command edge connects");
    }
    let mut coord = b.build().expect("the autonomy + slot graph builds");

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
// The slot's registered descriptor
// ---------------------------------------------------------------------------

/// The slot registers an extended occupant descriptor: the occupant's own
/// ports come first with the mount-appended tail (`SlotControlIn` after its
/// inputs, `SequenceStatus` after its outputs), followed by the runner tail
/// of a command fan-in, a `SequenceStatus` self-tap, and the `SlotStatus`
/// and events Host outputs.
#[test]
fn registered_descriptor_is_the_extended_occupant_shape() {
    use metor_fsw_2::metor_proto::types::Msg;
    use metor_fsw_2::{Delivery, FanIn, Frame, PortConn, PortId, SequenceStatus as SeqStatusFrame};
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None).unwrap();
    let d = b.descriptor_of(slot);
    assert_eq!(d.name, "adcs", "registered under the slot's instance name");

    // The fixture has no user ports, so the shape is exactly the implicit
    // occupant prefix plus the runner tail.
    let ins: Vec<(&str, PortConn)> = d.inputs.iter().map(|p| (p.name.as_str(), p.conn)).collect();
    assert_eq!(ins.len(), 3, "{ins:?}");
    assert_eq!(ins[0], ("slot_control", PortConn::Host), "{ins:?}");
    assert_eq!(ins[1].0, "SequenceCommand");
    assert_eq!(ins[1].1, PortConn::Edge);
    assert_eq!(d.inputs[1].fan_in, FanIn::Many);
    assert_eq!(d.inputs[1].delivery, Delivery::Log);
    assert_eq!(
        ins[2].1,
        PortConn::SelfTap(PortId::Component(SeqStatusFrame::FRAME_ID)),
        "{ins:?}"
    );

    let outs: Vec<(&str, PortConn)> = d.outputs.iter().map(|p| (p.name.as_str(), p.conn)).collect();
    assert_eq!(outs.len(), 5, "{outs:?}");
    // The occupant prefix is the entry's own outputs (health/log tail) with
    // the mount-appended SequenceStatus after them.
    assert_eq!(outs[0], ("health", PortConn::Edge), "{outs:?}");
    assert_eq!(outs[1], ("log", PortConn::Edge), "{outs:?}");
    assert_eq!(
        outs[2],
        ("sequence", PortConn::Edge),
        "mount-appended status: {outs:?}"
    );
    assert_eq!(outs[3], ("slot_status", PortConn::Host), "{outs:?}");
    // The events channel keys "<slot>.sequences" and stays telemetered.
    assert_eq!(outs[4], ("sequences", PortConn::Host), "{outs:?}");
    assert_eq!(
        d.outputs[4].id(),
        PortId::Packet(metor_fsw_2::metor_proto_wkt::SequenceChannelEvent::ID)
    );
    assert!(d.outputs[4].telemetered, "the events channel is downlinked");
}

#[test]
fn edge_into_a_host_connected_input_is_rejected() {
    #[allow(unused_imports)]
    use metor_fsw_2::Frame;
    use metor_fsw_2::{SequenceStatus as SeqStatusFrame, WireError};
    let Some(lib) = locate_fixture() else {
        return;
    };

    // Slot a's occupant-bound SequenceStatus output shares its PortId with slot
    // b's SequenceStatus self-tap input, the only edge-addressable non-Edge
    // input, so this connect must fail as a HostPort rather than bind a foreign
    // producer into a runner-held view.
    let mut b = Coordinator::builder(sim_config());
    let a = b.add_slot("a", vec![occ("waiter", open_waiter(&lib))], None).unwrap();
    let bslot = b.add_slot("b", vec![occ("waiter", open_waiter(&lib))], None).unwrap();
    b.connect(
        PortRef::new::<SeqStatusFrame>(a),
        PortRef::new::<SeqStatusFrame>(bslot),
    )
    .expect("push_edge only checks ids");
    let err = b
        .build()
        .err()
        .expect("an edge into a self-tap input fails");
    match err {
        WireError::HostPort { system, .. } => assert_eq!(system, "b"),
        other => panic!("expected HostPort, got {other:?}"),
    }
}

/// An initial occupant outside the allowed set is rejected as soon as the slot
/// is added.
#[test]
fn builder_initial_occupant_outside_allowed_set_is_rejected() {
    use metor_fsw_2::{InitialOccupant, SlotConfigError};
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let err = b
        .add_slot(
            "adcs",
            vec![occ("waiter", loaded)],
            Some(InitialOccupant::loaded("nonesuch")),
        )
        .expect_err("an initial occupant outside the allowed set is a registration error");
    assert!(matches!(err, SlotConfigError::UnknownInitial { .. }), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Uplink multi-output routing
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use metor_fsw_2::metor_proto_wkt::ReloadSequences;

static RELOADS_SEEN: AtomicU64 = AtomicU64::new(0);
static CMDS_SEEN: AtomicU64 = AtomicU64::new(0);

/// A cyclic consumer of both uplink command types, each over its own edge.
struct CmdTap;

#[derive(metor_fsw_2::SystemInput)]
struct CmdTapIn {
    reloads: metor_fsw_2::MsgIn<ReloadSequences>,
    commands: metor_fsw_2::MsgIn<SequenceCommand>,
}

#[derive(metor_fsw_2::SystemOutput)]
struct CmdTapOut {}

impl metor_fsw_2::System for CmdTap {
    type Input = CmdTapIn;
    type Output = metor_fsw_2::Out<CmdTapOut>;
    const NAME: &'static str = "cmd_tap";
}

impl metor_fsw_2::CyclicSystem for CmdTap {
    fn execute(
        &mut self,
        _now: metor_fsw_2::Timestamp,
        input: &mut CmdTapIn,
        _o: &mut Self::Output,
    ) {
        input.reloads.drain(|_| {
            RELOADS_SEEN.fetch_add(1, Relaxed);
        });
        input.commands.drain(|_| {
            CMDS_SEEN.fetch_add(1, Relaxed);
        });
    }
}

/// Received Msgs route by their declared output id. A second declared command
/// type gets its own output, an unknown id counts on the uplink's health, and
/// a malformed payload under a known id is dropped with the link staying up.
#[test]
fn uplink_routes_by_declared_output_and_survives_garbage() {
    use metor_fsw_2::metor_proto::types::LenPacket;

    RELOADS_SEEN.store(0, Relaxed);
    CMDS_SEEN.store(0, Relaxed);

    // The script is a second-type command (routes to `reloads`), an id the
    // uplink declares no output for (SequenceChannelEvent is downlink traffic),
    // a malformed payload under a known id, and finally a valid
    // SequenceCommand, which arrives only if the garbage did not kill the
    // reader.
    let unroutable = wire_msg(&SequenceChannelEvent {
        channel: "adcs".to_string(),
        kind: SequenceEventKind::Started,
    });
    let malformed = {
        // A SequenceCommand-id Msg whose payload cannot postcard-decode; its
        // string-length varint points far past the end.
        let mut pkt = LenPacket::msg(SequenceCommand::ID, 4);
        pkt.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        pkt.inner[4..].to_vec()
    };
    let valid = wire_msg(&SequenceCommand {
        channel: "anything".to_string(),
        command: SequenceCommandKind::Start,
    });
    let reload = wire_msg(&ReloadSequences {});

    let mut b = Coordinator::builder(sim_config());
    let tap = b.add_cyclic(CmdTap);
    let uplink = b.add_async(
        UplinkSystem::new(MockRecv::from_packets(vec![
            reload, unroutable, malformed, valid,
        ]))
        .with_msg::<ReloadSequences>()
        .with_msg::<SequenceCommand>(),
    );
    b.connect(
        PortRef::msg::<ReloadSequences>(uplink),
        PortRef::msg::<ReloadSequences>(tap),
    )
    .expect("the reloads edge connects");
    b.connect(
        PortRef::msg::<SequenceCommand>(uplink),
        PortRef::msg::<SequenceCommand>(tap),
    )
    .expect("the commands edge connects");
    let mut coord = b.build().expect("the uplink + tap graph builds");

    let mut health: Input<metor_fsw_2::SystemHealth> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("uplink.health"))
            .expect("the uplink's health is registered")
            .expect("reader slot available"),
    );

    let coord = stellarator::run(|| async move {
        // Plenty of cycles for the async uplink to drain its 4-packet script.
        coord.run_for(40).await;
        coord
    });

    assert_eq!(
        RELOADS_SEEN.load(Relaxed),
        1,
        "the second declared command type routed to its own output"
    );
    assert_eq!(
        CMDS_SEEN.load(Relaxed),
        1,
        "the valid command after the malformed one arrived; the link stayed up \
         and the garbage payload was dropped"
    );
    let mut errors = 0;
    let _ = health.drain(|f| errors = errors.max(f.get().errors));
    assert!(
        errors >= 1,
        "the unknown-id Msg bumped the uplink's unroutable counter: {errors}"
    );

    drop((coord, health));
}

// ---------------------------------------------------------------------------
// A plain cyclic entry as a slot occupant
// ---------------------------------------------------------------------------

/// The occupant tail is a mount property, so a slot loads an ordinary cyclic
/// entry (the fixture's fn-style `beater`). Shared plumbing for the two
/// cyclic-occupant tests (a stellarator run is once-per-thread, so the
/// steady phase and the abort are separate tests).
fn open_beater_in(lib: &PathBuf) -> DlSystem {
    let loaded = DlPack::open(lib)
        .expect("open the fixture pack")
        .system("beater")
        .expect("select the cyclic entry");
    // A cyclic entry carries no occupant tail of its own.
    assert_eq!(loaded.descriptor().inputs.len(), 0);
    assert_eq!(loaded.descriptor().outputs.len(), 2, "health + log only");
    loaded
}

fn build_beater_slot(loaded: DlSystem) -> Coordinator {
    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("beater", loaded)], None).unwrap();
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    b.build().expect("a cyclic occupant builds")
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
    let mut coord = build_beater_slot(open_beater_in(&lib));
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
    let mut coord = build_beater_slot(open_beater_in(&lib));
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
