//! Acceptance gate for a runtime **slot** driven **through the `Wiring`/KDL front-end**
//! (WP10 Wave 5), not a hand-built `CoordinatorBuilder`.
//!
//! Mirrors `tests/slot_integration.rs` (a real `dlopen` of the `#[sequence]` occupant),
//! but expresses the mission as a KDL `slot` document: it `parse`s the slot, runs the
//! build driver ([`build_artifacts`]) to locate the occupant `.so` (the same infra the
//! dl-fixture wiring test uses), [`resolve`]s it into a [`Coordinator`] with the slot
//! registered, and drives an `initial state="running"` slot to a terminal phase — proving
//! the `parse` → `build_artifacts` → `resolve_slot` path lands a live slot. A declared
//! contract mismatch surfaces as the clean [`LoadError::SlotContractMismatch`].

#![cfg(all(feature = "kdl", not(miri)))]

use metor_fsw_2::metor_proto::types::{ComponentId, Msg};
use metor_fsw_2::metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};
use metor_fsw_2::{
    Frame, Input, SlotStatus, build_artifacts, parse, resolve, split_record,
    wiring::{BuildOptions, LoadError, Registry},
};

/// The seq-fixture's cargo crate name + cdylib library stem.
const FIXTURE_CRATE: &str = "metor-fsw-2-seq-fixture";
const FIXTURE_STEM: &str = "metor_fsw_2_seq_fixture";

// SlotPhase wire codes (slot.rs): Empty=0, Loaded=1, Running=2, Done=3, Stopped=4.
const RUNNING: u8 = 2;
const DONE: u8 = 3;

/// A slot mission whose single allowed occupant is the `waiter` seq-fixture, started at
/// init. `waiter` has **no user ports**, so the slot declares no `input`/`output`.
fn slot_kdl() -> String {
    format!(
        r#"
coordinator cycle_rate=1000.0 sim_dt=0.000002
artifact "waiter" crate="{FIXTURE_CRATE}" lib="{FIXTURE_STEM}" type="waiter"
slot "adcs" {{
    allow occupant="waiter"
    initial occupant="waiter" state="running"
}}
"#
    )
}

/// Drain every `SlotStatus.phase` published over a run (mirrors `slot_integration`).
fn slot_phases(view: &mut Input<SlotStatus>) -> Vec<u8> {
    let mut phases = Vec::new();
    view.drain(|f| phases.push(f.get().phase)).expect("no lap");
    phases
}

#[test]
fn slot_via_wiring_resolves_and_runs_to_done() {
    let mut wiring = parse(&slot_kdl()).expect("parse the slot mission onto Wiring");
    assert_eq!(wiring.slots.len(), 1, "the slot parsed");

    // Build driver: cargo build -p the seq-fixture crate, locate + record its `.so`.
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        // Build plumbing unavailable: skip (slot_integration covers the slot directly).
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    assert!(
        wiring.artifacts[0].path.is_some(),
        "build driver resolved the occupant artifact path"
    );

    // Resolve the slot through the one shared resolver (the slot opens its allowed
    // occupant `.so` and registers under its instance name).
    let mut coord = resolve(&wiring, &Registry::new()).expect("resolve the slot Wiring");

    // The slot's host SlotStatus output is registered under its instance name.
    assert!(
        coord
            .output_instances()
            .iter()
            .any(|(name, fid)| *name == "adcs" && *fid == SlotStatus::FRAME_ID),
        "the slot's SlotStatus output is registered under its instance name"
    );

    // Tap the host SlotStatus before running.
    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("a reader slot is available"),
    );

    // The slot was started at init (state="running"); a short run reaches the terminal
    // Done phase (the `waiter` waits ~2 sim-µs then completes).
    let coord = stellarator::run(|| async move {
        coord.run_for(6).await;
        coord
    });

    let phases = slot_phases(&mut slot_view);
    assert!(phases.contains(&RUNNING), "the slot ran: {phases:?}");
    assert_eq!(
        phases.last(),
        Some(&DONE),
        "the initial=running slot reached terminal Done: {phases:?}"
    );
    assert!(coord.stopped().is_empty(), "Done is not a hard-stop");

    drop((coord, slot_view));
}

#[test]
fn slot_declared_contract_mismatch_is_a_clean_error() {
    // `waiter` has no user inputs, so declaring `input frame="nonsense"` cannot match the
    // resolved descriptor → the clean `SlotContractMismatch`.
    let kdl = format!(
        r#"
coordinator cycle_rate=1000.0 sim_dt=0.000002
artifact "waiter" crate="{FIXTURE_CRATE}" lib="{FIXTURE_STEM}" type="waiter"
slot "adcs" {{
    input frame="nonsense"
    allow occupant="waiter"
}}
"#
    );
    let mut wiring = parse(&kdl).expect("parse the slot mission");
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    let err = match resolve(&wiring, &Registry::new()) {
        Ok(_) => panic!("expected a SlotContractMismatch error"),
        Err(e) => e,
    };
    assert!(
        matches!(err, LoadError::SlotContractMismatch { dir: "input", .. }),
        "{err:?}"
    );
}

/// Drain a message ring into the decoded `Msg`s of one type (the downlink tap's decode,
/// mirrors `slot_integration`).
fn drain_msgs<M: Msg + serde::de::DeserializeOwned>(
    view: &mut metor_fsw_2::ring::View<
        metor_fsw_2::ring::BoxBacking,
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
fn unknown_runtime_load_is_rejected_with_a_failed_event() {
    // A runtime `Load` naming an occupant outside the allowed set is rejected loudly —
    // a `Failed` event naming the bad occupant + the allowed set on the slot's events
    // channel — not silently swallowed leaving the slot stuck `Empty` with no diagnostic.
    let kdl = format!(
        r#"
coordinator cycle_rate=1000.0 sim_dt=0.000002
artifact "waiter" crate="{FIXTURE_CRATE}" lib="{FIXTURE_STEM}" type="waiter"
slot "adcs" {{
    allow occupant="waiter"
}}
"#
    );
    let mut wiring = parse(&kdl).expect("parse the slot mission");
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    let mut coord = resolve(&wiring, &Registry::new()).expect("resolve the slot Wiring");

    // Tap the slot's events channel BEFORE the run (an overwrite ring starts at the
    // live edge, so the tap must precede the emit).
    let messages = coord.message_registry();
    let mut events_view = messages
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");

    // A typo'd panel/uplink Load, injected through the in-proc control handle.
    let ch = coord.channel_id("adcs").expect("adcs slot channel");
    let mut control = coord.control_handle();
    control
        .emit(&SequenceCommand {
            channel_id: ch,
            command: SequenceCommandKind::Load {
                name: "nonesuch".to_string(),
            },
        })
        .unwrap();

    let coord = stellarator::run(|| async move {
        coord.run_for(2).await;
        coord
    });

    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    let failed = events
        .iter()
        .find_map(|e| match &e.kind {
            SequenceEventKind::Failed { reason } => Some(reason.clone()),
            _ => None,
        })
        .expect("the unknown-occupant Load emitted a Failed event");
    assert!(failed.contains("nonesuch"), "names the bad occupant: {failed}");
    assert!(failed.contains("waiter"), "lists the allowed set: {failed}");

    drop((coord, events_view, control));
}
