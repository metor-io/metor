//! Runtime slots driven through the KDL wiring front end.
//!
//! Each test starts from a KDL `slot` document rather than a hand-built
//! coordinator. The document is [`parse`]d, the build driver
//! ([`build_artifacts`]) compiles and locates the occupant shared library, and
//! [`resolve`] turns the whole thing into a live [`Coordinator`]. The tests
//! then observe the slot from the outside, through its host `SlotStatus`
//! frames, its events channel, and its user outputs.
//!
//! Every test skips (with a note on stderr) if the build driver cannot compile
//! the fixture crate, so the suite stays runnable in environments without the
//! fixture toolchain.

#![cfg(all(feature = "kdl", not(miri)))]

use metor_fsw_2::metor_proto::types::{ComponentId, Msg};
use metor_fsw_2::metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};
use metor_fsw_2::{
    Frame, Input, SlotStatus, split_record,
    wiring::{BuildOptions, LoadError, Registry, build_artifacts, parse, resolve},
};

/// The seq fixture's cargo crate name and cdylib library stem.
const FIXTURE_CRATE: &str = "metor-fsw-2-seq-fixture";
const FIXTURE_STEM: &str = "metor_fsw_2_seq_fixture";

// SlotState wire codes (SlotState::code): Empty=0, Loaded=1, Running=2, Done=3, Stopped=4.
const RUNNING: u8 = 2;
const DONE: u8 = 3;

/// A slot mission whose single allowed occupant is the `waiter` fixture,
/// started at init. `waiter` has no user ports, so the slot declares no
/// `input` or `output`.
fn slot_kdl() -> String {
    format!(
        r#"
coordinator cycle_rate=1000.0 sim_dt=0.000002
artifact "waiter" crate="{FIXTURE_CRATE}" lib="{FIXTURE_STEM}"
slot "adcs" {{
    allow occupant="waiter"
    initial occupant="waiter" state="running"
}}
"#
    )
}

/// Drain every `SlotStatus.phase` published over a run.
fn slot_phases(view: &mut Input<SlotStatus>) -> Vec<u8> {
    let mut phases = Vec::new();
    view.drain(|f| phases.push(f.get().phase)).expect("no lap");
    phases
}

#[test]
fn slot_via_wiring_resolves_and_runs_to_done() {
    let mut wiring = parse(&slot_kdl()).expect("parse the slot mission onto Wiring");
    assert_eq!(wiring.slots.len(), 1, "the slot parsed");

    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    assert!(
        wiring.artifacts[0].path.is_some(),
        "build driver resolved the occupant artifact path"
    );

    let mut coord = resolve(&wiring, &Registry::new()).expect("resolve the slot Wiring");

    // The slot's host SlotStatus output is registered under its instance name.
    assert!(
        coord
            .output_instances()
            .iter()
            .any(|(name, fid)| *name == "adcs" && *fid == SlotStatus::FRAME_ID),
        "the slot's SlotStatus output is registered under its instance name"
    );

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("a reader slot is available"),
    );

    // The slot was started at init (state="running"); a short run reaches the
    // terminal Done phase, since the waiter waits about 2 sim-microseconds and
    // then completes.
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

/// A `system "alarms"` node coexists with a slot, and is declared first on
/// purpose. The alarm engine registers a receive-all input, which must come
/// last in build order; the resolver defers it behind the slot's cyclic
/// registration so the document still builds. The alarm targets the slot's own
/// host `SlotStatus.phase` frame and raises when the occupant completes
/// (Done = 3 > 2.5): to the alarm engine a slot is just another telemetered
/// component.
#[test]
fn alarms_node_before_a_slot_still_builds_and_raises() {
    let kdl = format!(
        r#"
coordinator cycle_rate=1000.0 sim_dt=0.000002
artifact "waiter" crate="{FIXTURE_CRATE}" lib="{FIXTURE_STEM}"
system "alarms" type="Alarms" {{
    alarm id="SLOT_DONE" name="Slot Done" {{
        target component="adcs.slot_status.phase"
        warning above=2.5
    }}
}}
slot "adcs" {{
    allow occupant="waiter"
    initial occupant="waiter" state="running"
}}
"#
    );
    let mut wiring = parse(&kdl).expect("parse the alarmed slot mission");
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }

    let coord = resolve(&wiring, &Registry::with_builtins())
        .expect("the alarm engine's receive-all input defers behind the slot");
    let mut raised: metor_fsw_2::MsgIn<metor_fsw_2::metor_proto_wkt::AlarmRaised> =
        metor_fsw_2::MsgIn::new(
            coord
                .registry()
                .get(ComponentId::new("alarms.AlarmRaised"))
                .expect("alarm raise channel registered")
                .view()
                .expect("a reader slot is available"),
        );

    let coord = stellarator::run(|| async move {
        let mut coord = coord;
        coord.run_for(6).await;
        coord
    });

    let mut got = Vec::new();
    raised.drain(|r| got.push(r.def_id));
    assert_eq!(got, vec!["SLOT_DONE"], "the slot-phase alarm raised once");
    drop(coord);
}

#[test]
fn slot_declared_contract_mismatch_is_a_clean_error() {
    // `waiter` has no user inputs, so declaring `input frame="nonsense"` cannot
    // match the resolved descriptor and resolve reports SlotContractMismatch.
    let kdl = format!(
        r#"
coordinator cycle_rate=1000.0 sim_dt=0.000002
artifact "waiter" crate="{FIXTURE_CRATE}" lib="{FIXTURE_STEM}"
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

/// Drain a message ring, decoding every record as one `Msg` type.
fn drain_msgs<M: Msg + serde::de::DeserializeOwned>(
    view: &mut metor_fsw_2::ring::View<metor_fsw_2::ring::NoWake, metor_fsw_2::ring::NoWake>,
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

#[test]
fn unknown_runtime_load_is_rejected_with_a_failed_event() {
    // A runtime `Load` naming an occupant outside the allowed set is rejected
    // loudly, with a `Failed` event on the slot's events channel naming the bad
    // occupant and the allowed set, rather than silently leaving the slot Empty
    // with no diagnostic.
    let kdl = format!(
        r#"
coordinator cycle_rate=1000.0 sim_dt=0.000002
artifact "waiter" crate="{FIXTURE_CRATE}" lib="{FIXTURE_STEM}"
slot "adcs" {{
    allow occupant="waiter"
}}
// The in-proc control handle reaches the slot only over this explicit edge;
// "coordinator" is the reserved instance name of the coordinator's own bundle.
connect "coordinator" -> "adcs" msg="SequenceCommand"
"#
    );
    let mut wiring = parse(&kdl).expect("parse the slot mission");
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    let mut coord = resolve(&wiring, &Registry::new()).expect("resolve the slot Wiring");

    // Tap the events channel before the run; an overwrite ring starts a reader
    // at the live edge, so a tap taken after the emit would miss it.
    let messages = coord.registry();
    let mut events_view = messages
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");

    // A typo'd Load, injected through the in-proc control handle and addressed
    // to the slot by its instance name.
    let mut control = coord.control_handle().expect("taken once per coordinator");
    control
        .emit(&SequenceCommand {
            channel: "adcs".to_string(),
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
    assert!(
        failed.contains("nonesuch"),
        "names the bad occupant: {failed}"
    );
    assert!(failed.contains("waiter"), "lists the allowed set: {failed}");

    drop((coord, events_view, control));
}

// --- Occupant params on `allow` ---

/// The parametered seq fixture's cargo crate name and cdylib library stem.
const PARAM_FIXTURE_CRATE: &str = "metor-fsw-2-seq-param-fixture";
const PARAM_FIXTURE_STEM: &str = "metor_fsw_2_seq_param_fixture";

/// Host-side mirror of the fixture's `GainerParams` (the typed builder path).
#[derive(serde::Serialize)]
struct GainerParams {
    gain: f64,
}

/// Host-side mirror of the fixture's `gain_out` frame, byte for byte.
#[derive(
    Frame,
    zerocopy::IntoBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
    zerocopy::FromBytes,
    Default,
)]
#[repr(C)]
#[metor_fsw(name = "gain_out")]
struct GainOut {
    #[metor_fsw(timestamp)]
    timestamp: metor_fsw_2::metor_proto::types::Timestamp,
    gain: f64,
}

/// A slot mission whose single allowed occupant carries params, spelled by
/// `allow_line` either as line properties or as a child block.
fn param_slot_kdl(allow_line: &str) -> String {
    format!(
        r#"
coordinator cycle_rate=1000.0 sim_dt=0.000002
artifact "gainer" crate="{PARAM_FIXTURE_CRATE}" lib="{PARAM_FIXTURE_STEM}"
slot "gslot" {{
    output frame="gain_out"
    {allow_line}
    initial occupant="gainer" state="running"
}}
"#
    )
}

/// Occupant params on `allow` resolve and run in both KDL spellings, line
/// properties and a child block. Both must schema-encode to exactly the
/// postcard bytes the typed `allow_with_params` builder produces, and the
/// running occupant must observably apply them (the fixture republishes its
/// configured gain).
#[test]
fn slot_allow_params_resolve_and_run_b1() {
    use metor_fsw_2::wiring::encode_kdl_params;
    use metor_fsw_2::{DlPack, ParamSource, SlotInitState, WiringBuilder};

    let line_form = param_slot_kdl(r#"allow occupant="gainer" gain=0.8"#);
    let child_form = param_slot_kdl(r#"allow occupant="gainer" { gain 0.8 }"#);

    let mut wiring = parse(&line_form).expect("parse the line-property allow params");
    let child_wiring = parse(&child_form).expect("parse the child-block allow params");

    // Both spellings must parse to carried KDL params.
    let carried = |w: &metor_fsw_2::Wiring| match &w.slots[0].allow[0].params {
        ParamSource::Kdl(text) => text.clone(),
        other => panic!("expected ParamSource::Kdl, got {other:?}"),
    };
    let line_text = carried(&wiring);
    let child_text = carried(&child_wiring);

    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    let so_path = wiring.artifacts[0]
        .path
        .clone()
        .expect("located fixture .so");

    // (a) Byte equality with the typed builder path.
    let builder_wiring = WiringBuilder::new()
        .coordinator(
            1000.0,
            metor_fsw_2::ClockSpec::Simulated { dt_secs: 0.000002 },
        )
        .artifact("gainer", PARAM_FIXTURE_CRATE, PARAM_FIXTURE_STEM)
        .slot("gslot")
        .output("gain_out")
        .allow_with_params("gainer", GainerParams { gain: 0.8 })
        .initial("gainer", SlotInitState::Running)
        .end()
        .build();
    let builder_bytes = match &builder_wiring.slots[0].allow[0].params {
        ParamSource::Postcard(b) => b.clone(),
        other => panic!("expected Postcard params from the builder, got {other:?}"),
    };
    let dl = DlPack::open(&so_path)
        .expect("open the fixture .so for its Params schema")
        .system("gainer")
        .expect("select the gainer entry");
    let encode = |text: &str| {
        encode_kdl_params(text, dl.params_schema(), "gslot", &["occupant"], 0)
            .expect("schema-encode the allow params")
    };
    assert_eq!(
        encode(&line_text),
        builder_bytes,
        "line-property form byte-matches"
    );
    assert_eq!(
        encode(&child_text),
        builder_bytes,
        "child-block form byte-matches"
    );
    drop(dl);

    // (b) End to end: the resolved slot runs and the occupant publishes the
    // params-derived gain on its user output.
    let mut coord = resolve(&wiring, &Registry::new()).expect("resolve the parametered slot");
    let mut gain_view: Input<GainOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("gslot.gain_out"))
            .expect("the slot's user output is registered")
            .expect("a reader slot is available"),
    );
    let coord = stellarator::run(|| async move {
        coord.run_for(6).await;
        coord
    });
    {
        let out = gain_view
            .latest()
            .expect("the occupant published its configured gain");
        assert_eq!(
            out.get().gain,
            0.8,
            "allow params reached the running occupant"
        );
    }
    assert!(coord.stopped().is_empty(), "Done is not a hard-stop");

    drop((coord, gain_view));
}

#[test]
fn slot_allow_unknown_param_is_a_clean_error() {
    // A typo'd allow param is a spanned UnknownParam at resolve, naming the
    // property, not a silent drop.
    let kdl = param_slot_kdl(r#"allow occupant="gainer" gian=0.8"#);
    let mut wiring = parse(&kdl).expect("parse the typo'd allow params");
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    let err = match resolve(&wiring, &Registry::new()) {
        Ok(_) => panic!("expected an UnknownParam error"),
        Err(e) => e,
    };
    assert!(
        matches!(err, LoadError::UnknownParam { ref property, .. } if property == "gian"),
        "{err:?}"
    );
}
