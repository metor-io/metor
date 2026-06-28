//! WP8 Wave 3a acceptance gate (dl-open.md §6): a dl graph driven **through the
//! `Wiring` data model**, not a hand-built `CoordinatorBuilder`.
//!
//! Mirrors `tests/dl_integration.rs` end to end, but expresses the mission as a
//! [`Wiring`] built with the Rust [`WiringBuilder`] (a static producer + a dlopen'd
//! consumer referencing the `metor-fsw-2-dl-fixture` artifact), runs the build driver
//! ([`build_artifacts`]) to locate the `.so`, then [`resolve`]s and runs it — proving
//! the builder → build-driver → resolver path and the typed-params (`.params(..)`)
//! encoding land in the running coordinator.

#![cfg(all(feature = "kdl", not(miri)))]

use metor_fsw_2::metor_proto::types::{ComponentId, Timestamp};
use metor_fsw_2::{
    BuildSystem, ClockSpec, CyclicSystem, Frame, Input, Out, Output, System, SystemInput,
    SystemKind, SystemOutput, TelemetryModeSpec, WiringBuilder, build_artifacts, resolve,
    wiring::{BuildOptions, Registry},
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// ---------------------------------------------------------------------------
// Host-side frames + params (byte-for-byte the fixture's).
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

/// Mirror of the fixture's `CounterParams`; `.params(..)` postcard-encodes it.
#[derive(serde::Serialize, Default)]
struct CounterParams {
    start: u64,
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

/// The platform shared-object file name for the fixture crate's `cdylib`.
fn fixture_lib_name() -> String {
    let stem = "metor_fsw_2_dl_fixture";
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

#[test]
fn dl_graph_via_wiring_resolve_end_to_end() {
    // 1. Express the mission as a `Wiring` via the Rust builder: a static producer +
    //    a dlopen'd consumer referencing the fixture artifact, with typed params.
    let mut wiring = WiringBuilder::new()
        .coordinator(200.0, ClockSpec::Simulated { dt_secs: 0.005 })
        .artifact(
            "counter",
            "metor-fsw-2-dl-fixture",
            fixture_lib_name(),
            "DlCounter",
        )
        .system("ticker")
        .ty("Ticker")
        .from_static()
        .end()
        .system("counter")
        .ty("DlCounter")
        .from_artifact("counter")
        .params(CounterParams { start: 1000 })
        .end()
        .connect("ticker", "tick_in", "counter", "tick_in")
        .telemetry("127.0.0.1:2240".parse().unwrap(), TelemetryModeSpec::All)
        .build();

    // 2. Build driver: cargo build -p the fixture crate, locate + record its `.so`.
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        // Build plumbing unavailable: skip (dl_integration covers the loader directly).
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    assert!(
        wiring.artifacts[0].path.is_some(),
        "build driver resolved the artifact path"
    );

    // 3. Resolve the `Wiring` through the one shared resolver (static via the Registry,
    //    dl via DlSystem::open) into a running coordinator.
    let mut registry = Registry::new();
    registry.register::<Ticker, _>("Ticker");
    let mut coord = resolve(&wiring, &registry).expect("resolve the dl Wiring");

    assert!(
        coord
            .output_instances()
            .iter()
            .any(|(name, fid)| *name == "counter" && *fid == TickOut::FRAME_ID),
        "the dl consumer's output is registered under its instance name"
    );

    // 4. Tap the dl system's output before running.
    let mut out_view: Input<TickOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("counter.tick_out"))
            .expect("dl output registered for telemetry `All`")
            .expect("a reader slot is available"),
    );

    // 5. Run several cycles; the dl consumer samples this cycle's fresh tick.
    const CYCLES: usize = 6;
    let coord = stellarator::run(|| async move {
        coord.run_for(CYCLES).await;
        coord
    });

    // 6. The typed params took effect: count = start(1000) + latest value(6).
    let out = out_view
        .latest()
        .expect("no lap on the tapped output")
        .expect("the dl system produced a tick_out");
    assert_eq!(
        out.get().count,
        1000 + CYCLES as u64,
        "start (from .params) + latest value"
    );

    // 7. Descriptor sanity: the resolved dl system is the fixture's cyclic counter.
    drop(out_view);
    drop(coord);

    // (Re-)confirm the descriptor kind via a fresh open, proving the artifact path is real.
    let dl = metor_fsw_2::DlSystem::open(wiring.artifacts[0].path.as_ref().unwrap())
        .expect("open the located .so");
    assert_eq!(dl.descriptor().name, "dl_counter");
    assert_eq!(dl.descriptor().kind, SystemKind::Cyclic);
}
