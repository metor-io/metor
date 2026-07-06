//! Slots/sequences acceptance gate (WP10 Wave 4): a **real `dlopen`** of a `#[sequence]`
//! occupant driven through a runtime **slot**.
//!
//! This builds the `metor-fsw-2-seq-fixture` crate as a `cdylib`, `dlopen`s the produced
//! shared object through [`DlSystem`], registers it as the allowed occupant of a slot in
//! a real [`Coordinator`], and drives the slot's lifecycle through
//! [`Coordinator::control_handle`] — proving the slot ring topology (the slot-owned
//! control ring appended to the occupant's input array + the `SlotStatus` output tap),
//! the command→phase state machine, and clean teardown across a genuine `.so` boundary.
//!
//! Slots/sequences are **ungated** (no `kdl`), so unlike `dl_integration` this test does
//! not need the wiring feature. As with `dl_integration` the fixture build runs inside
//! the test and the body is skipped (not failed) if the build plumbing is unavailable.

#![cfg(not(miri))]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use metor_fsw_2::metor_proto::types::{ComponentId, IntoLenPacket, Msg, OwnedPacket};
use metor_fsw_2::metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
    SequenceRegistry,
};
use metor_fsw_2::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorConfig, DlSystem, Input, PortRef,
    RecvTransport, SequenceStatus, SlotStatus, SystemKind, TransportError, split_record,
};

/// One paramless allowed occupant (the E8e `AllowedOccupant` shape).
fn occ(name: &str, system: DlSystem) -> AllowedOccupant {
    AllowedOccupant {
        name: name.to_string(),
        system,
        params: Vec::new(),
    }
}
use stellarator::buf::{IoBuf, Slice};

/// A `Load { occupant }` command addressed to the channel named `ch` (the slot's
/// instance name) — the in-proc twin of a panel uplink.
fn load(ch: &str, occupant: &str) -> SequenceCommand {
    SequenceCommand {
        channel: ch.to_string(),
        command: SequenceCommandKind::Load {
            name: occupant.to_string(),
        },
    }
}

/// A non-`Load` command (`Start`/`Stop`/`Abort`/`Reset`) addressed to the channel named `ch`.
fn cmd(ch: &str, command: SequenceCommandKind) -> SequenceCommand {
    SequenceCommand {
        channel: ch.to_string(),
        command,
    }
}

// ---------------------------------------------------------------------------
// Build + locate the sequence fixture cdylib (mirrors dl_integration).
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

/// `cargo build -p metor-fsw-2-seq-fixture` and return the produced shared object's path
/// (parsed from cargo's JSON artifact messages). `None` (with a stderr note) if the build
/// plumbing is unavailable, so the caller skips rather than fails spuriously.
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
        // 2µs per cycle: the `waiter` future's `wait(2µs)` elapses after one step.
        clock: ClockMode::Simulated {
            dt: Duration::from_micros(2),
        },
        ..CoordinatorConfig::default()
    }
}

/// Open the fixture once, asserting its reconstructed descriptor is a sequence with the
/// implicit `SlotControlIn` input + the `SequenceStatus`/health/log output tail.
fn open_waiter(lib: &PathBuf) -> DlSystem {
    let loaded = DlSystem::open(lib).expect("DlSystem::open the sequence .so");
    let desc = loaded.descriptor();
    assert_eq!(desc.name, "waiter");
    assert_eq!(desc.kind, SystemKind::Cyclic);
    // inputs = [SlotControlIn] (no user ports); outputs = [SequenceStatus, health, log].
    assert_eq!(desc.inputs.len(), 1, "just the implicit SlotControlIn input");
    assert_eq!(desc.outputs.len(), 3, "SequenceStatus + health + log tail");
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
    view.drain(|f| states.push(f.get().run_state)).expect("no lap");
    states
}

// SlotState wire codes (SlotState::code): Empty=0, Loaded=1, Running=2, Done=3, Stopped=4.
const LOADED: u8 = 1;
const RUNNING: u8 = 2;
const DONE: u8 = 3;
// Outcome::run_state codes (sequence/mod.rs): Completed=1, Aborted=2.
const COMPLETED: u8 = 1;
const ABORTED: u8 = 2;

// ---------------------------------------------------------------------------
// 1. Load → Start → run to Done (Completed), asserting the phase transitions and
//    the occupant's own SequenceStatus terminal run_state.
// ---------------------------------------------------------------------------

#[test]
fn slot_load_start_runs_to_done() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None);
    // A2: commands are explicit dataflow — the in-proc control handle reaches the
    // slot only over this declared edge.
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

    // Tap the host SlotStatus + the occupant SequenceStatus before running.
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

    // Drive Load + Start through the in-proc control handle (drained at cycle 0),
    // addressed by the slot's instance name.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control.emit(&cmd("adcs", SequenceCommandKind::Start)).unwrap();

    let coord = stellarator::run(|| async move {
        coord.run_for(4).await;
        coord
    });

    let phases = slot_phases(&mut slot_view);
    assert!(phases.contains(&RUNNING), "the slot ran: {phases:?}");
    assert_eq!(phases.last(), Some(&DONE), "the slot finished Done: {phases:?}");

    let states = seq_run_states(&mut seq_view);
    assert_eq!(
        states.last(),
        Some(&COMPLETED),
        "the occupant reached Completed: {states:?}"
    );

    // Done is a terminal success, not an error-stop: nothing in the stopped surface.
    assert!(coord.stopped().is_empty(), "Done is not a hard-stop");

    drop((coord, slot_view, seq_view, control));
}

// ---------------------------------------------------------------------------
// 2. Abort: a cooperative cancel reaches the occupant as ring data; it folds it at
//    its next wait and completes Done{Aborted}.
// ---------------------------------------------------------------------------

#[test]
fn slot_abort_completes_aborted() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None);
    // A2: commands are explicit dataflow — the in-proc control handle reaches the
    // slot only over this declared edge.
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

    // Load + Start + Abort, all drained at cycle 0 (before the wait deadline), so the
    // very first poll observes the cancel and bails out via the safing branch.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control.emit(&cmd("adcs", SequenceCommandKind::Start)).unwrap();
    control.emit(&cmd("adcs", SequenceCommandKind::Abort)).unwrap();

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
    assert_eq!(phases.last(), Some(&DONE), "Aborted is still terminal Done: {phases:?}");

    drop((coord, seq_view, slot_view, control));
}

// ---------------------------------------------------------------------------
// 3. Stop (hard-drop): the occupant's future is dropped (fsw_destroy releases its
//    ring roles), leaving the slot Loaded with no live future — it never polls.
// ---------------------------------------------------------------------------

#[test]
fn slot_stop_hard_drops_occupant() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None);
    // A2: commands are explicit dataflow — the in-proc control handle reaches the
    // slot only over this declared edge.
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

    // Load + Start + Stop: the hard-drop returns the slot to Loaded with the future gone.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control.emit(&cmd("adcs", SequenceCommandKind::Start)).unwrap();
    control.emit(&cmd("adcs", SequenceCommandKind::Stop)).unwrap();

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
    // The occupant was destroyed before it ever polled, so it published no SequenceStatus.
    assert!(
        seq_run_states(&mut seq_view).is_empty(),
        "a hard-dropped occupant never executed"
    );
    assert!(coord.stopped().is_empty(), "a hard-drop is not an error-stop");

    drop((coord, slot_view, seq_view, control));
}

// ---------------------------------------------------------------------------
// 4. Reset: after a terminal Done, Reset rebuilds the occupant from the start and a
//    fresh Start runs it to completion again (the slot reloads over the same rings).
// ---------------------------------------------------------------------------

#[test]
fn slot_reset_reruns_from_start() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None);
    // A2: commands are explicit dataflow — the in-proc control handle reaches the
    // slot only over this declared edge.
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

    // One bounded run (a coordinator drives exactly one `run_for`; a rerun would
    // re-init everything). Cycles 1-3: Load + Start → Completed. An injector task —
    // polled at the Simulated loop's per-cycle yield — then emits Reset (rebuild the
    // selected occupant over the same rings) + Start, which the slot drains at the
    // head of cycle 4; cycles 4-6 reload and complete again.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control.emit(&cmd("adcs", SequenceCommandKind::Start)).unwrap();
    let progress = coord.progress();
    let coord = stellarator::run(|| async move {
        let injector = stellarator::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            while progress.load(Relaxed) < 3 {
                stellarator::yield_now().await;
            }
            control.emit(&cmd("adcs", SequenceCommandKind::Reset)).unwrap();
            control.emit(&cmd("adcs", SequenceCommandKind::Start)).unwrap();
        });
        coord.run_for(6).await;
        let _ = injector.await;
        coord
    });

    // Two completed runs across the reload: at least two terminal Completed records.
    let states = seq_run_states(&mut seq_view);
    let completed = states.iter().filter(|&&s| s == COMPLETED).count();
    assert!(
        completed >= 2,
        "the occupant completed once per run across the Reset reload: {states:?}"
    );

    drop((coord, seq_view));
}

// ---------------------------------------------------------------------------
// 5. The sequence coupling (Wave 4): the slot's message channel emits ordered
//    SequenceChannelEvents (Loaded → Started → Progress* → Completed, no coalescing)
//    and the coordinator emits a boot SequenceRegistry listing the slot + its allowed
//    occupants. Taps the message rings via `message_registry()`.
// ---------------------------------------------------------------------------

/// Drain a message ring into the decoded `Msg`s of one type, asserting the 2-byte id and
/// postcard-round-tripping each record (the downlink tap's decode, `docs/messages.md` §3).
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
fn slot_emits_ordered_sequence_events_and_boot_registry() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None);
    // A2: commands are explicit dataflow — the in-proc control handle reaches the
    // slot only over this declared edge.
    b.connect(
        PortRef::msg::<SequenceCommand>(b.coordinator_handle()),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the coordinator command edge connects");
    let mut coord = b.build().expect("the slot graph builds");

    // Tap the slot's events channel + the coordinator's boot-registry channel BEFORE the
    // run — an overwrite ring starts at the live edge, so the taps must precede the emits.
    let messages = coord.registry();
    let mut events_view = messages
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");
    let mut boot_view = messages
        .view(ComponentId::new("coordinator.sequences"))
        .expect("the coordinator boot-registry channel is registered")
        .expect("reader slot available");

    // Drive Load + Start (drained at cycle 0), run to the occupant's Completed.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control.emit(&load("adcs", "waiter")).unwrap();
    control.emit(&cmd("adcs", SequenceCommandKind::Start)).unwrap();
    let mut coord = coord;
    let coord = stellarator::run(|| async move {
        coord.run_for(4).await;
        coord
    });

    // The boot SequenceRegistry lists the slot with its allowed-occupant set.
    let registries = drain_msgs::<SequenceRegistry>(&mut boot_view);
    let registry = registries.last().expect("a boot SequenceRegistry was emitted");
    assert_eq!(registry.channels.len(), 1, "one slot channel");
    assert_eq!(registry.channels[0].name, "adcs");
    assert_eq!(registry.channels[0].available, vec!["waiter".to_string()]);

    // The slot's events, IN ORDER, with no coalescing: Loaded → Started → Progress* →
    // Completed (the `waiter` occupant emits "waiting" then "done").
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
    // The progress lines arrive in order, between Started and Completed.
    let waiting = kinds
        .iter()
        .position(|k| matches!(k, SequenceEventKind::Progress { detail } if detail == "waiting"))
        .expect("a Progress{waiting} event");
    let done = kinds
        .iter()
        .position(|k| matches!(k, SequenceEventKind::Progress { detail } if detail == "done"))
        .expect("a Progress{done} event");
    assert!(1 < waiting && waiting < done && done < kinds.len() - 1, "Progress ordered: {kinds:?}");

    drop((coord, events_view, boot_view, control));
}

// ---------------------------------------------------------------------------
// 6. The uplink (Wave 3): a panel `SequenceCommand` injected through a mock
//    `RecvTransport` lands in the slot command ring at the head of the cycle and
//    dispatches the SAME cycle (no off-by-one) — driving an initially-empty slot
//    Empty → Loaded → Running with no observable bare-Loaded cycle.
// ---------------------------------------------------------------------------

/// A mock [`RecvTransport`] yielding a fixed script of pre-encoded wire packets, then a
/// `Disconnected` (the reader stops, like a dropped link). Each packet is real wire
/// bytes re-parsed, so the loopback exercises the same `OwnedPacket::Msg` / route path
/// the TCP reader uses.
struct MockRecv {
    queue: std::collections::VecDeque<Vec<u8>>,
}

/// Encode one Msg to its framed wire bytes, stripping the `LenPacket` 4-byte length
/// prefix — the framing the panel sends and `OwnedPacket::parse` expects.
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

    /// A script of arbitrary pre-encoded packets (the A8 multi-type/garbage cases).
    fn from_packets(packets: Vec<Vec<u8>>) -> Self {
        Self {
            queue: packets.into(),
        }
    }
}

impl RecvTransport for MockRecv {
    async fn recv(&mut self) -> Result<OwnedPacket<Slice<Vec<u8>>>, TransportError> {
        match self.queue.pop_front() {
            Some(bytes) => {
                let slice = bytes.try_slice(..).expect("non-empty packet");
                OwnedPacket::parse(slice).map_err(|e| TransportError::Io(Box::new(e)))
            }
            None => Err(TransportError::Disconnected),
        }
    }
}

#[test]
fn uplink_command_loads_and_starts_same_cycle() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    // One slot, started EMPTY (no initial occupant) — the interactive panel scenario.
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None);
    // The uplink: a panel `Load { waiter }` then `Start`, both addressed to the slot by
    // its instance name — and reaching it over an explicit command edge (A2).
    let uplink = b.add_uplink(MockRecv::new(vec![
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
    ]));
    b.connect(
        PortRef::msg::<SequenceCommand>(uplink),
        PortRef::msg::<SequenceCommand>(slot),
    )
    .expect("the uplink command edge connects");
    let mut coord = b.build().expect("the slot + uplink graph builds");

    // Tap the host SlotStatus before running (an overwrite ring starts at the live edge).
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
    // The slot ran: the uplink Load + Start drove it past Empty into Running (and on to
    // Done, as the `waiter` occupant completes).
    assert!(
        phases.contains(&RUNNING),
        "the uplink drove the slot to Running: {phases:?}"
    );
    assert_eq!(
        phases.last(),
        Some(&DONE),
        "the started occupant ran to Done: {phases:?}"
    );
    // No off-by-one: Load and Start dispatched the *same* cycle, so the slot never
    // published a bare `Loaded` phase (which a one-cycle-late Start would have shown).
    assert!(
        !phases.contains(&LOADED),
        "Load and Start landed the same cycle (no bare Loaded): {phases:?}"
    );

    drop((coord, slot_view));
}

// ---------------------------------------------------------------------------
// 7. Name addressing (design-command-slots.md §2.3): the slot's instance name IS the
//    wire address, so it is validated at build (the NAME_CAP), a command naming no
//    slot is dropped by every slot's filter, and reordering slot declarations does
//    not re-address commands (the ChannelId regression).
// ---------------------------------------------------------------------------

#[test]
fn slot_name_over_the_cap_is_a_build_error() {
    use metor_fsw_2::{NAME_CAP, WireError};
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    // One byte over the cap: would telemeter truncated while addressing untruncated.
    let long = "a".repeat(NAME_CAP + 1);
    let mut b = Coordinator::builder(sim_config());
    let _slot = b.add_slot(long.clone(), vec![occ("waiter", loaded)], None);
    let err = b.build().err().expect("an over-cap slot name fails the build");
    match err {
        WireError::SlotNameTooLong { name, len } => {
            assert_eq!(name, long);
            assert_eq!(len, NAME_CAP + 1);
        }
        other => panic!("expected SlotNameTooLong, got {other:?}"),
    }
}

#[test]
fn misaddressed_command_matches_no_slot() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None);
    // A2: commands are explicit dataflow — the in-proc control handle reaches the
    // slot only over this declared edge.
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

    // A command for another channel and one whose name exceeds the cap: both simply
    // match no slot (dropped by the per-slot name filter — no panic, no state change).
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

/// The ChannelId regression body: commands used to address the slot's build-order index,
/// so swapping two slot declarations silently re-targeted ground commands. Build a
/// two-slot mission in the given declaration order; the command addressed to "adcs" must
/// drive the adcs slot either way, and the recovery slot must never leave Empty. (One
/// order per `#[test]` — `stellarator::run` is once-per-thread.)
fn drive_adcs_of_two_slots(adcs_first: bool) {
    let Some(lib) = locate_fixture() else {
        return;
    };

    let mut b = Coordinator::builder(sim_config());
    let add = |b: &mut metor_fsw_2::CoordinatorBuilder, name: &str| {
        let slot = b.add_slot(
            name,
            vec![occ("waiter", open_waiter(&lib))],
            None,
        );
        // Fan-out: the ONE producer is edged to BOTH slots; only the slot the
        // command's `channel` names may act (name addressing makes broad fan-out
        // harmless).
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
// 8. The A2 headline: command edges are explicit dataflow. A system that merely
//    *declares* a `MsgOut<SequenceCommand>` output commands nothing — only an
//    explicit `connect … msg="SequenceCommand"` edge lets its emits reach a slot.
// ---------------------------------------------------------------------------

/// A stand-in autonomy emitter: a cyclic system that emits `Load { waiter }` +
/// `Start` for the `adcs` channel on its first cycle.
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

/// Build one slot + the autonomy emitter, with or without the command edge, and
/// return the phases the slot published over a short run.
fn autonomy_phases(edged: bool) -> Option<Vec<u8>> {
    let lib = locate_fixture()?;
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let autonomy = b.add_cyclic(Autonomy { sent: false });
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None);
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

#[test]
fn command_output_without_an_edge_commands_nothing() {
    // The old type-keyed collection would have broadcast the emitter's commands to
    // every slot; with explicit edges an un-edged producer is inert.
    let Some(phases) = autonomy_phases(false) else {
        return;
    };
    assert!(
        phases.iter().all(|&p| p == 0),
        "an un-edged SequenceCommand producer drives no slot: {phases:?}"
    );
}

#[test]
fn command_output_with_an_edge_drives_the_slot() {
    // The same emit, now over a declared edge, drives the slot to completion.
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
// 9. A3 — the slot's registered descriptor is derived by extension: occupant
//    prefix (SlotControlIn re-marked Host in place), runner tail (commands fan-in,
//    SequenceStatus self-tap; SlotStatus + "sequences" Host outputs). Host/SelfTap
//    inputs reject edges; the builder path validates the initial occupant (W1b).
// ---------------------------------------------------------------------------

#[test]
fn registered_descriptor_is_the_extended_occupant_shape() {
    use metor_fsw_2::{
        Delivery, FanIn, Frame, PortConn, PortId, SequenceStatus as SeqStatusFrame,
    };
    use metor_fsw_2::metor_proto::types::Msg;
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let mut b = Coordinator::builder(sim_config());
    let slot = b.add_slot("adcs", vec![occ("waiter", loaded)], None);
    let d = b.descriptor_of(slot);
    assert_eq!(d.name, "adcs", "registered under the slot's instance name");

    // waiter has no user ports: inputs = [slot_control (Host), commands (Edge,
    // fan-in), sequence self-tap]; outputs = [sequence, health, log (occupant
    // prefix), slot_status (Host), sequences events (Host)].
    let ins: Vec<(&str, PortConn)> = d.inputs.iter().map(|p| (p.name, p.conn)).collect();
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

    let outs: Vec<(&str, PortConn)> = d.outputs.iter().map(|p| (p.name, p.conn)).collect();
    assert_eq!(outs.len(), 5, "{outs:?}");
    assert_eq!(outs[0], ("sequence", PortConn::Edge), "occupant prefix: {outs:?}");
    assert_eq!(outs[1], ("health", PortConn::Edge), "{outs:?}");
    assert_eq!(outs[2], ("log", PortConn::Edge), "{outs:?}");
    assert_eq!(outs[3], ("slot_status", PortConn::Host), "{outs:?}");
    // The events channel keys "<slot>.sequences" and stays telemetered.
    assert_eq!(outs[4], ("sequences", PortConn::Host), "{outs:?}");
    assert_eq!(
        d.outputs[4].id,
        PortId::Packet(metor_fsw_2::metor_proto_wkt::SequenceChannelEvent::ID)
    );
    assert!(d.outputs[4].telemetered, "the events channel is downlinked");
}

#[test]
fn edge_into_a_host_connected_input_is_rejected() {
    use metor_fsw_2::{SequenceStatus as SeqStatusFrame, WireError};
    #[allow(unused_imports)]
    use metor_fsw_2::Frame;
    let Some(lib) = locate_fixture() else {
        return;
    };

    // Slot A's occupant-bound SequenceStatus output shares its PortId with slot B's
    // SequenceStatus SELF-TAP input — the only edge-addressable non-Edge input — so
    // this connect must fail as a HostPort, never bind a foreign producer into a
    // runner-held view.
    let mut b = Coordinator::builder(sim_config());
    let a = b.add_slot("a", vec![occ("waiter", open_waiter(&lib))], None);
    let bslot = b.add_slot("b", vec![occ("waiter", open_waiter(&lib))], None);
    b.connect(
        PortRef::new::<SeqStatusFrame>(a),
        PortRef::new::<SeqStatusFrame>(bslot),
    )
    .expect("push_edge only checks ids");
    let err = b.build().err().expect("an edge into a self-tap input fails");
    match err {
        WireError::HostPort { system, .. } => assert_eq!(system, "b"),
        other => panic!("expected HostPort, got {other:?}"),
    }
}

#[test]
fn builder_initial_occupant_outside_allowed_set_panics() {
    use metor_fsw_2::InitialOccupant;
    use std::panic::AssertUnwindSafe;
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    // W1b: the builder path validates the initial occupant against the allowed set
    // at build (the KDL path surfaces the same as UnknownInitialOccupant).
    let mut b = Coordinator::builder(sim_config());
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        b.add_slot(
            "adcs",
            vec![occ("waiter", loaded)],
            Some(InitialOccupant::loaded("nonesuch")),
        )
    }));
    assert!(
        result.is_err(),
        "an initial occupant outside the allowed set is a build-time contract panic"
    );
}

// ---------------------------------------------------------------------------
// 10. A8 — uplink multi-output dispatch: received Msgs route by declared output
//     id (the RouteMsg seam); a second declared command type Just Works; an
//     unknown id bumps `uplink.unroutable`; a malformed payload for a known id
//     is dropped with the link staying up.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use metor_fsw_2::metor_proto_wkt::ReloadSequences;

static RELOADS_SEEN: AtomicU64 = AtomicU64::new(0);
static CMDS_SEEN: AtomicU64 = AtomicU64::new(0);

/// A cyclic consumer of BOTH uplink command types, each over its own explicit edge.
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

#[test]
fn uplink_routes_by_declared_output_and_survives_garbage() {
    use metor_fsw_2::metor_proto::types::LenPacket;

    RELOADS_SEEN.store(0, Relaxed);
    CMDS_SEEN.store(0, Relaxed);

    // The script: a second-type command (routes to `reloads`), an id the uplink
    // declares no output for (unroutable — SequenceChannelEvent is downlink
    // traffic), a malformed payload under a KNOWN id (dropped; the link must stay
    // up), then a valid SequenceCommand — received only if the garbage did not
    // kill the reader.
    let unroutable = wire_msg(&SequenceChannelEvent {
        channel: "adcs".to_string(),
        kind: SequenceEventKind::Started,
    });
    let malformed = {
        // A SequenceCommand-id Msg whose payload cannot postcard-decode (a string
        // length varint pointing far past the end).
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
    let uplink = b.add_uplink(MockRecv::from_packets(vec![
        reload, unroutable, malformed, valid,
    ]));
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

    // Tap the uplink's health BEFORE the run (overwrite rings start at the live edge).
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
        "the valid command AFTER the malformed one arrived — the link stayed up \
         and the garbage payload was dropped"
    );
    // The unroutable id was counted on uplink health (`uplink.unroutable`).
    let mut errors = 0;
    let _ = health.drain(|f| errors = errors.max(f.get().errors));
    assert!(
        errors >= 1,
        "the unknown-id Msg bumped the uplink's unroutable counter: {errors}"
    );

    drop((coord, health));
}
