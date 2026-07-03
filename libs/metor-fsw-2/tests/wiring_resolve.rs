//! Acceptance gate for a dl graph driven **through the `Wiring` data model**, not a
//! hand-built `CoordinatorBuilder`.
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
    BuildSystem, ClockSpec, CyclicSystem, DlSystem, Frame, Input, Out, Output, ParamSource, System,
    SystemInput, SystemKind, SystemOutput, TelemetryModeSpec, Wiring, WiringBuilder,
    wiring::{BuildOptions, Registry, build_artifacts, encode_kdl_params, parse, resolve},
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

/// The library **stem** for the fixture crate's `cdylib` (both front-ends take a stem;
/// the framework decorates it to the platform file name via `cdylib_file_name`).
fn fixture_lib_stem() -> &'static str {
    "metor_fsw_2_dl_fixture"
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
            fixture_lib_stem(),
            "DlCounter",
        )
        .system("ticker")
        .ty("Ticker")
        .from_static()
        .end()
        .system("counter")
        .ty("DlCounter")
        .from_artifact("counter")
        .params(CounterParams { start: 1000, scale: 1.0 })
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

/// Headline equivalence: configure the **same** dl system two ways — the typed Rust
/// `WiringBuilder.params(..)` and a KDL `system "..." artifact=".."
/// start=.. scale=..` — and assert BOTH (a) resolve to **byte-identical** `Params` bytes
/// and (b) run to the **same** output. The KDL path is schema-encoded from the `.so`'s
/// exported `Params` schema, so the host never links the fixture's `CounterParams`.
#[test]
fn kdl_and_builder_dl_params_are_byte_identical_and_run_equal() {
    // --- The Rust-builder front-end -------------------------------------------------
    let mut builder_wiring = WiringBuilder::new()
        .coordinator(200.0, ClockSpec::Simulated { dt_secs: 0.005 })
        .artifact("counter", "metor-fsw-2-dl-fixture", fixture_lib_stem(), "DlCounter")
        .system("ticker")
        .ty("Ticker")
        .from_static()
        .end()
        .system("counter")
        .ty("DlCounter")
        .from_artifact("counter")
        .params(CounterParams { start: 1000, scale: 2.0 })
        .end()
        .connect("ticker", "tick_in", "counter", "tick_in")
        .build();

    // --- The KDL front-end (the SAME mission, params as node properties) ------------
    let kdl = format!(
        r#"
coordinator cycle_rate=200.0 sim_dt=0.005
artifact "counter" crate="metor-fsw-2-dl-fixture" lib="{lib}" type="DlCounter"
system "ticker" type="Ticker"
system "counter" type="DlCounter" artifact="counter" start=1000 scale=2.0
connect "ticker" -> "counter" frame="tick_in"
"#,
        lib = fixture_lib_stem()
    );
    let mut kdl_wiring = parse(&kdl).expect("parse the KDL mission onto Wiring");

    // Build the fixture once; share the located `.so` path with both wirings.
    if let Err(e) = build_artifacts(&mut builder_wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    kdl_wiring.artifacts[0].path = builder_wiring.artifacts[0].path.clone();

    // --- (a) Byte-identical `SystemSpec` params -------------------------------------
    // The builder system carries postcard bytes directly; the KDL system carries its node
    // text, schema-encoded here against the `.so`'s exported schema (the resolve path).
    let path = builder_wiring.artifacts[0].path.as_ref().unwrap();
    let builder_bytes = match &builder_wiring.systems[1].params {
        ParamSource::Postcard(b) => b.clone(),
        other => panic!("builder dl system should carry Postcard params, got {other:?}"),
    };
    let kdl_text = match &kdl_wiring.systems[1].params {
        ParamSource::Kdl(t) => t.clone(),
        other => panic!("KDL dl system should carry Kdl params, got {other:?}"),
    };
    let kdl_bytes = {
        let dl = DlSystem::open(path).expect("open the fixture .so for its Params schema");
        encode_kdl_params(&kdl_text, dl.params_schema(), "counter", &["type", "artifact"], 1)
            .expect("schema-encode KDL params")
    };
    assert_eq!(
        builder_bytes, kdl_bytes,
        "KDL ≡ Rust-builder params on the wire (the one-postcard-encoding invariant)"
    );

    // --- (b) Both resolve + run to the same output ----------------------------------
    // Resolve both (sync) and tap each output, then drive both in **one** `stellarator::run`
    // (the process has a single executor).
    let resolve_one = |wiring: &Wiring| {
        let mut registry = Registry::new();
        registry.register::<Ticker, _>("Ticker");
        resolve(wiring, &registry).expect("resolve the dl Wiring")
    };
    let tap = |coord: &metor_fsw_2::Coordinator| -> Input<TickOut> {
        Input::new(
            coord
                .registry()
                .view(ComponentId::new("counter.tick_out"))
                .expect("dl output registered")
                .expect("a reader slot is available"),
        )
    };
    let builder_coord = resolve_one(&builder_wiring);
    let kdl_coord = resolve_one(&kdl_wiring);
    let mut builder_view = tap(&builder_coord);
    let mut kdl_view = tap(&kdl_coord);

    let (builder_coord, kdl_coord) = stellarator::run(|| async move {
        let mut bc = builder_coord;
        let mut kc = kdl_coord;
        bc.run_for(6).await;
        kc.run_for(6).await;
        (bc, kc)
    });

    let count_of = |view: &mut Input<TickOut>| -> u64 {
        view.latest().expect("a tick_out was produced").get().count
    };
    let builder_count = count_of(&mut builder_view);
    let kdl_count = count_of(&mut kdl_view);
    // start(1000) + value(6) * scale(2.0) = 1012, both front-ends.
    assert_eq!(builder_count, 1012, "builder front-end output");
    assert_eq!(kdl_count, builder_count, "KDL front-end runs to the same output");

    drop((builder_view, kdl_view, builder_coord, kdl_coord));
}

/// E5a: `type=` is optional on a dl system (the artifact's `system_type` is
/// authoritative) — the mission resolves and runs without it; and an explicit
/// `type=` contradicting the artifact is the clean spanned `TypeMismatchesArtifact`.
#[test]
fn dl_type_optional_and_validated_against_artifact() {
    use metor_fsw_2::wiring::LoadError;

    let kdl = format!(
        r#"
coordinator cycle_rate=200.0 sim_dt=0.005
artifact "counter" crate="metor-fsw-2-dl-fixture" lib="{lib}" type="DlCounter"
system "ticker" type="Ticker"
system "counter" artifact="counter" start=5 scale=1.0
connect "ticker" -> "counter" frame="tick_in"
"#,
        lib = fixture_lib_stem()
    );
    let mut wiring = parse(&kdl).expect("type= is optional with artifact=");
    assert_eq!(wiring.systems[1].ty, None, "no explicit type on the dl system");

    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }

    let mut registry = Registry::new();
    registry.register::<Ticker, _>("Ticker");
    let coord = resolve(&wiring, &registry).expect("a type-less dl system resolves");
    drop(coord);

    // A `type=` that contradicts the artifact's exported type is rejected.
    let mut bad = wiring.clone();
    bad.systems[1].ty = Some("WrongType".to_string());
    let err = match resolve(&bad, &registry) {
        Ok(_) => panic!("expected TypeMismatchesArtifact"),
        Err(e) => e,
    };
    match err {
        LoadError::TypeMismatchesArtifact { system, ty, artifact_type, .. } => {
            assert_eq!(system, "counter");
            assert_eq!(ty, "WrongType");
            assert_eq!(artifact_type, "DlCounter");
        }
        other => panic!("expected TypeMismatchesArtifact, got {other:?}"),
    }
}
