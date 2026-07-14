//! End-to-end tests for driving a dl graph through the [`Wiring`] data model.
//!
//! Each test describes a mission as data with the Rust [`WiringBuilder`]. The
//! mission wires a statically linked producer into a dlopen'd consumer loaded
//! from the `metor-fsw-2-dl-fixture` artifact. The tests then run
//! [`build_artifacts`] to compile the fixture and locate its shared library,
//! [`resolve`] the wiring into a coordinator, and run it, checking that outputs
//! appear under their instance names and that params reach the loaded system.
//!
//! When the fixture cannot be built (no cargo, no fixture crate on disk) the
//! tests skip rather than fail.

#![cfg(all(feature = "wiring", not(miri)))]

use metor_fsw_2::metor_proto::types::{ComponentId, Timestamp};
use metor_fsw_2::{
    BuildSystem, ClockSpec, CyclicSystem, DlPack, Frame, Input, Out, Output, System, SystemInput,
    SystemKind, SystemOutput, WiringBuilder,
    wiring::{BuildOptions, Registry, build_artifacts, resolve},
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// ---------------------------------------------------------------------------
// Host-side frames and params, byte-for-byte the fixture's layouts.
// ---------------------------------------------------------------------------

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_in")]
struct TickIn {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    value: u64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_out")]
struct TickOut {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    count: u64,
}

/// Host-side copy of the fixture's params struct, for `.params(..)` to encode.
#[derive(serde::Serialize, Default)]
struct CounterParams {
    start: u64,
    scale: f64,
}

// ---------------------------------------------------------------------------
// A statically-linked producer feeding the dlopen'd consumer.
// ---------------------------------------------------------------------------

struct Ticker {
    n: u64,
}

#[derive(SystemInput)]
struct TickerIn {}

#[derive(SystemOutput)]
struct TickerOut {
    tick: Output<TickIn>,
}

impl System for Ticker {
    type Input = TickerIn;
    type Output = Out<TickerOut>;
    const NAME: &'static str = "ticker";
}

impl CyclicSystem for Ticker {
    fn execute(&mut self, now: Timestamp, _input: &mut TickerIn, output: &mut Out<TickerOut>) {
        self.n += 1;
        let _ = output.tick.write(&TickIn {
            timestamp: now,
            value: self.n,
        });
    }
}

impl BuildSystem for Ticker {
    type Params = ();
    fn new(_params: ()) -> Self {
        Ticker { n: 0 }
    }
}

/// Library stem of the fixture's `cdylib`; the framework turns it into the
/// platform file name.
fn fixture_lib_stem() -> &'static str {
    "metor_fsw_2_dl_fixture"
}

#[test]
fn dl_graph_via_wiring_resolve_end_to_end() {
    // Describe the mission with the Rust builder: a static producer and a
    // dlopen'd consumer with typed params.
    let mut wiring = WiringBuilder::new()
        .coordinator(200.0, ClockSpec::Simulated { dt_secs: 0.005 })
        .artifact("counter", "metor-fsw-2-dl-fixture", fixture_lib_stem())
        .system("ticker")
        .ty("Ticker")
        .end()
        .system("counter")
        .ty("DlCounter")
        .from_artifact("counter")
        .params(CounterParams {
            start: 1000,
            scale: 1.0,
        })
        .end()
        .connect("ticker", "tick_in", "counter", "tick_in")
        .telemetry("127.0.0.1:2240".parse().unwrap())
        .build();

    // Build the fixture crate and record where its shared library landed.
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    assert!(
        wiring.artifacts[0].path.is_some(),
        "build driver resolved the artifact path"
    );

    // Resolve into a coordinator; static systems come from the registry, the
    // dl system from its artifact.
    let mut registry = Registry::with_builtins();
    registry.register::<Ticker, _>("Ticker");
    let mut coord = resolve(&wiring, &registry).expect("resolve the dl Wiring");

    assert!(
        coord
            .output_instances()
            .iter()
            .any(|(name, fid)| *name == "counter" && *fid == TickOut::FRAME_ID),
        "the dl consumer's output is registered under its instance name"
    );

    // Tap the dl system's output before running.
    let mut out_view: Input<TickOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("counter.tick_out"))
            .expect("dl output registered for telemetry `All`")
            .expect("a reader slot is available"),
    );

    // Run several cycles; each cycle the consumer samples that cycle's tick.
    const CYCLES: usize = 6;
    let coord = stellarator::run(|| async move {
        coord.run_for(CYCLES).await;
        coord
    });

    // The typed params took effect: count = start(1000) + latest value(6).
    {
        let out = out_view
            .latest()
            .expect("the dl system produced a tick_out");
        assert_eq!(
            out.get().count,
            1000 + CYCLES as u64,
            "start (from .params) + latest value"
        );
    }

    drop(out_view);
    drop(coord);

    // A fresh open of the recorded path yields the fixture's pack, so the
    // located artifact is real.
    let pack = DlPack::open(wiring.artifacts[0].path.as_ref().unwrap())
        .expect("open the located .so");
    assert_eq!(
        pack.system_names().collect::<Vec<_>>(),
        ["DlCounter", "DlEcho"]
    );
    let dl = pack.system("DlCounter").expect("select the counter entry");
    assert_eq!(dl.descriptor().name, "DlCounter");
    assert_eq!(dl.descriptor().kind, SystemKind::Cyclic);
}

/// `type=` selects the pack entry a dl system instantiates. Over a
/// multi-entry pack an omitted `type=` is a clean `PackTypeRequired` error
/// listing the choices, and a `type=` naming no exported entry fails with
/// `PackSystem` wrapping [`DlError::UnknownPackSystem`].
#[test]
fn dl_type_selects_the_pack_entry_and_unknown_type_is_rejected() {
    use metor_fsw_2::DlError;
    use metor_fsw_2::wiring::LoadErrorKind;

    // A dl system with `artifact=` but no `type=`: the builder leaves the type
    // unset, so resolve must pick the pack entry itself.
    let mut wiring = WiringBuilder::new()
        .coordinator(200.0, ClockSpec::Simulated { dt_secs: 0.005 })
        .artifact("counter", "metor-fsw-2-dl-fixture", fixture_lib_stem())
        .system("ticker")
        .ty("Ticker")
        .end()
        .system("counter")
        .from_artifact("counter")
        .params_value(serde_json::json!({ "start": 5, "scale": 1.0 }))
        .end()
        .connect("ticker", "tick_in", "counter", "tick_in")
        .build();
    assert_eq!(
        wiring.systems[1].ty, None,
        "no explicit type on the dl system"
    );

    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }

    // The fixture pack exports two entries, so the type-less spec cannot pick
    // one; resolve reports the choices instead of guessing.
    let mut registry = Registry::new();
    registry.register::<Ticker, _>("Ticker");
    let err = match resolve(&wiring, &registry) {
        Ok(_) => panic!("expected PackTypeRequired"),
        Err(e) => e,
    };
    match err.kind {
        LoadErrorKind::PackTypeRequired {
            system, available, ..
        } => {
            assert_eq!(system, "counter");
            assert_eq!(available, "DlCounter, DlEcho");
        }
        other => panic!("expected PackTypeRequired, got {other:?}"),
    }

    // A `type=` the pack does not export is rejected, naming the exports.
    let mut bad = wiring.clone();
    bad.systems[1].ty = Some("WrongType".to_string());
    let err = match resolve(&bad, &registry) {
        Ok(_) => panic!("expected PackSystem"),
        Err(e) => e,
    };
    match err.kind {
        LoadErrorKind::PackSystem { system, source, .. } => {
            assert_eq!(system, "counter");
            match *source {
                DlError::UnknownPackSystem { name, available } => {
                    assert_eq!(name, "WrongType");
                    assert_eq!(available, ["DlCounter", "DlEcho"]);
                }
                other => panic!("expected UnknownPackSystem, got {other:?}"),
            }
        }
        other => panic!("expected PackSystem, got {other:?}"),
    }

    // Naming a real entry resolves and runs.
    let mut good = wiring.clone();
    good.systems[1].ty = Some("DlCounter".to_string());
    let coord = resolve(&good, &registry).expect("an exported type= resolves");
    drop(coord);
}
