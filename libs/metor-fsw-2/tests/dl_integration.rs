//! End-to-end test of the shared-object loader, driven through [`Wiring`].
//!
//! The `tests_abi` suite calls the generated `fsw_*` symbols inside the test
//! binary itself; this test crosses a real shared-object boundary. It builds
//! the `metor-fsw-2-dl-fixture` crate as a `cdylib`, describes the target as a
//! [`Wiring`] (a static producer plus one or two dlopen'd consumers), points
//! the artifact at the freshly built `.so`, and [`resolve`]s it into a live
//! [`Coordinator`]. Running a few cycles exercises the loader, the manifest
//! round-trip, the ring-region hand-off, the per-cycle drive, the telemetry
//! tap, and teardown.
//!
//! The fixture is built by a nested `cargo build` inside the test, which is
//! safe because the outer `cargo test` lock is released before the test binary
//! runs. If the build cannot produce a usable shared object,
//! [`locate_fixture`] returns `None` and the test skips with a message on
//! stderr instead of failing; `tests_abi` keeps the loader logic covered
//! regardless.

#![cfg(not(miri))]

use std::path::PathBuf;

mod common;
use common::{CounterParams, TickEvent, TickIn, TickOut, Ticker};

use metor_fsw_2::metor_proto::types::{ComponentId, Msg};
use metor_fsw_2::{
    ClockSpec, CoordinatorSpec, Delivery, DlPack, FanIn, Frame, Input, MsgIn, ParamSource, PortId,
    StopReason, SystemKind, WiringBuilder,
    wiring::{BuildOptions, Registry, provision_artifacts, resolve},
};

// ---------------------------------------------------------------------------
// Build and locate the fixture cdylib
// ---------------------------------------------------------------------------

/// Build the fixture crate and locate its `cdylib`, skipping on failure.
fn locate_fixture() -> Option<PathBuf> {
    common::locate_fixture("metor-fsw-2-dl-fixture", "metor_fsw_2_dl_fixture")
}

/// A 200 Hz, depth-8, 5 ms-per-step simulated target, the shared coordinator
/// config across the dl tests.
fn dl_coordinator() -> CoordinatorSpec {
    CoordinatorSpec {
        cycle_rate: 200.0,
        default_depth: Some(8),
        clock: ClockSpec::Simulated { dt_secs: 0.005 },
        namespace: None,
        wasm_fuel_per_poll: None,
        wasm_memory_limit_bytes: None,
    }
}

/// A registry with `Ticker` registered as the static producer.
fn ticker_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register::<Ticker, _>("Ticker");
    registry
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[test]
fn dlopen_cyclic_system_end_to_end() {
    let Some(lib_path) = locate_fixture() else {
        // Build plumbing unavailable; tests_abi covers the loader in-binary.
        return;
    };

    // 1. Open the shared object directly to check the pack's exported entry
    //    names and validate the selected entry's reconstructed descriptor —
    //    the same manifest `resolve` reads back through its own open.
    let pack = DlPack::open(&lib_path).expect("DlPack::open the fixture .so");
    assert_eq!(
        pack.system_names().collect::<Vec<_>>(),
        ["DlCounter", "DlEcho"],
        "the pack manifest lists both entries in registration order"
    );
    let loaded = pack.system("DlCounter").expect("select the counter entry");
    let desc = loaded.descriptor();
    assert_eq!(desc.name, "DlCounter");
    assert_eq!(desc.kind, SystemKind::Cyclic);
    assert_eq!(desc.inputs.len(), 1, "one user input (tick_in)");
    assert_eq!(
        desc.inputs[0].id().component().expect("table port"),
        TickIn::FRAME_ID
    );
    // User `out`, user `events` (the Postcard port), implicit log.
    assert_eq!(desc.outputs.len(), 3);
    assert_eq!(
        desc.outputs[0].id().component().expect("table port"),
        TickOut::FRAME_ID
    );
    // The message port crossed the ABI schema-tagged with its axes intact; a
    // shared object declares a Postcard port exactly like a static system.
    let events = &desc.outputs[1];
    assert_eq!(events.id(), PortId::Packet(TickEvent::ID));
    assert_eq!(
        events.name, "TickEvent",
        "the NamedMsg token survives the wire"
    );
    assert_eq!(events.delivery, Delivery::Log);
    assert_eq!(events.fan_in, FanIn::Many);
    assert!(events.telemetered);
    assert!(
        desc.capabilities.is_empty(),
        "a .so can hold no host capabilities"
    );
    drop((loaded, pack));

    // 2. Describe the target: a static producer into two instances of the
    //    same loaded entry, each with its own params (start=1000, start=2000).
    //    One opened pack mints any number of independent instances.
    let mut wiring = WiringBuilder::new()
        .coordinator_spec(dl_coordinator())
        .artifact(
            "counter",
            "metor-fsw-2-dl-fixture",
            "metor_fsw_2_dl_fixture",
        )
        .system("ticker")
        .ty("Ticker")
        .end()
        .system("dl_counter")
        .ty("DlCounter")
        .from_artifact("counter")
        .params(CounterParams {
            start: 1000,
            scale: 1.0,
        })
        .end()
        .system("dl_counter_b")
        .ty("DlCounter")
        .from_artifact("counter")
        .params(CounterParams {
            start: 2000,
            scale: 1.0,
        })
        .end()
        // Each `connect` runs `compatible()` over the loaded descriptor's
        // `tick_in` input, the same wiring validation a static system gets.
        .connect("ticker", "tick_in", "dl_counter", "tick_in")
        .connect("ticker", "tick_in", "dl_counter_b", "tick_in")
        .build();
    // `main`'s helper already built and located the fixture; fill the path in
    // place of the build driver.
    wiring.artifacts[0].path = Some(lib_path);

    let mut coord =
        resolve(&wiring, &ticker_registry()).expect("graph resolves (validation + sizing + bind)");

    // 3. Tap the loaded system's output through the telemetry registry before
    //    running; a fresh view only sees records committed from now on.
    let mut out_view: Input<TickOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("dl_counter.tick_out"))
            .expect("dl output is registered for telemetry `All`")
            .expect("a reader slot is available"),
    );
    // The Postcard port is a registered message channel like any static
    // system's, keyed `<instance>.<NAME>` and drained with a host-side MsgIn.
    let mut events_in: MsgIn<TickEvent> = MsgIn::new(
        coord
            .registry()
            .view(ComponentId::new("dl_counter.TickEvent"))
            .expect("dl message channel is registered")
            .expect("a reader slot is available"),
    );
    let mut twin_view: Input<TickOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("dl_counter_b.tick_out"))
            .expect("second instance's output is registered")
            .expect("a reader slot is available"),
    );
    let mut log_in: MsgIn<metor_fsw_2::LogEvent> = MsgIn::new(
        coord
            .registry()
            .view(ComponentId::new("dl_counter.log"))
            .expect("the implicit log channel is registered")
            .expect("a reader slot is available"),
    );

    // 4. Run several cycles. The producer runs before the consumer each cycle,
    //    so the consumer samples that cycle's fresh value. `run_for` performs
    //    init (fsw_pack_bind_init), the cycles (fsw_pack_execute), and shutdown
    //    (fsw_pack_shutdown); `coord` is handed back so it outlives the reads below.
    const CYCLES: usize = 6;
    let coord = stellarator::run(|| async move {
        coord.run_for(CYCLES).await;
        coord
    });

    // 5. The loaded system produced correct output: count = start + value.
    let last_out_ts = {
        let out = out_view
            .latest()
            .expect("tick_out ring readable")
            .expect("the dl system produced a tick_out");
        assert_eq!(
            out.get().count,
            1000 + CYCLES as u64,
            "start + latest value"
        );
        out.get().timestamp
    };

    // 5a. The second instance of the same entry ran independently, with its
    //     own params.
    {
        let out = twin_view
            .latest()
            .expect("tick_out ring readable")
            .expect("the second dl instance produced a tick_out");
        assert_eq!(
            out.get().count,
            2000 + CYCLES as u64,
            "the twin's own start + latest value"
        );
    }

    // 5b. The Postcard port carried every cycle's event, in order, decoded
    //     purely from the self-describing id.
    let mut counts: Vec<u64> = Vec::new();
    events_in
        .drain(|e| counts.push(e.count))
        .expect("events ring readable");
    assert_eq!(
        counts,
        (1..=CYCLES as u64).map(|v| 1000 + v).collect::<Vec<_>>(),
        "every-record log semantics across the dl boundary"
    );

    // 5c. The fixture's `tracing::info!` inside execute crossed the boundary
    //     too: the export shim installs a per-dylib subscriber, and each
    //     instance's log flush drains the dylib's queue onto its own log
    //     port, re-stamped with the instance name.
    let mut traced: Vec<metor_fsw_2::LogEvent> = Vec::new();
    log_in
        .drain(|ev| traced.push(ev))
        .expect("log ring readable");
    let ev = traced
        .iter()
        .find(|ev| ev.message == "tick counted")
        .expect("the pack's tracing event reached its log port");
    assert_eq!(ev.source, "dl_counter", "attributed to the instance");
    // Born on the FSW clock: the ABI shim republishes each step's `now`
    // inside the dylib, so the last cycle's event carries exactly the cycle
    // timestamp the last tick_out frame does.
    assert!(
        traced.iter().any(|ev| ev.timestamp == last_out_ts),
        "traced events live on the simulated cycle clock, not wall time"
    );
    assert_eq!(ev.level, metor_fsw_2::LogLevel::Info);
    assert!(
        ev.fields.iter().any(|(k, _)| k == "count"),
        "the event's fields survive: {:?}",
        ev.fields
    );

    // 6. Teardown ordering: dropping the coordinator runs `fsw_pack_destroy` before
    //    the `Library` unloads and before the `RingTable` frees the regions.
    //    Not crashing here is the assertion.
    drop(coord);
    drop((out_view, events_in, twin_view, log_in));
}

/// Building through the driver ([`provision_artifacts`]) leaves a
/// `<cdylib>.manifest` sidecar next to the `.so` — raw postcard
/// [`PackManifest`] bytes naming the same entries the opened pack reports
/// — and `manifest_sidecar: false` opts out. Byte-level sidecar ≡ describe
/// equality is asserted in the driver's own unit tests
/// (`wiring::build_driver`), where the raw describe bytes are reachable.
///
/// [`provision_artifacts`]: metor_fsw_2::wiring::provision_artifacts
/// [`PackManifest`]: metor_fsw_2::abi::PackManifest
#[test]
fn build_driver_writes_manifest_sidecar() {
    let fixture = || {
        WiringBuilder::new()
            .artifact(
                "fixture",
                "metor-fsw-2-dl-fixture",
                "metor_fsw_2_dl_fixture",
            )
            .build()
    };
    let mut wiring = fixture();
    if let Err(e) = provision_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: provision_artifacts failed: {e}");
        return;
    }
    let so = wiring.artifacts[0].path.clone().expect("path filled");
    let mut sidecar = so.clone().into_os_string();
    sidecar.push(".manifest");
    let sidecar = PathBuf::from(sidecar);

    let bytes = std::fs::read(&sidecar).expect("sidecar written next to the built .so");
    let manifest: metor_fsw_2::abi::PackManifest =
        postcard::from_bytes(&bytes).expect("sidecar bytes decode as the pack manifest");
    let pack = DlPack::open(&so).expect("DlPack::open the fixture .so");
    assert_eq!(
        manifest
            .systems
            .iter()
            .map(|s| s.descriptor.name.as_str())
            .collect::<Vec<_>>(),
        pack.system_names().collect::<Vec<_>>(),
        "the sidecar names the entries the opened pack reports"
    );
    drop(pack);

    // Opting out builds the library but writes no sidecar.
    std::fs::remove_file(&sidecar).expect("clear the sidecar for the opt-out check");
    let opts = BuildOptions {
        manifest_sidecar: false,
        ..BuildOptions::default()
    };
    let mut wiring = fixture();
    provision_artifacts(&mut wiring, &opts).expect("rebuild is a cargo no-op");
    assert!(!sidecar.exists(), "opted out, so no sidecar is written");
}

/// A failing `fsw_pack_create` must not leave the slot looking permanently
/// `Running`. Here the params do not postcard-decode as `CounterParams`, so
/// the fixture's `catch_unwind` inside `fsw_pack_create` returns a null state. The
/// coordinator's stopped-systems telemetry must report the system from the
/// first cycle, exactly like a lapped input or an in-cycle panic would.
#[test]
fn dlopen_null_create_reports_stopped() {
    let Some(lib_path) = locate_fixture() else {
        // Build plumbing unavailable; tests_abi covers the loader in-binary.
        return;
    };

    // `CounterParams` is `{ start: u64, scale: f64 }`; three bytes cannot
    // decode as either field, so the fixture's `fsw_pack_create` panics inside its
    // own `catch_unwind` and returns a null state rather than crashing across
    // the `extern "C"` boundary (`abi_panic_is_contained` tests that
    // containment directly, in-binary). The raw bytes are set straight on the
    // spec's `ParamSource::Postcard`, which the dl path passes through verbatim.
    let mut wiring = WiringBuilder::new()
        .coordinator_spec(dl_coordinator())
        .artifact(
            "counter",
            "metor-fsw-2-dl-fixture",
            "metor_fsw_2_dl_fixture",
        )
        .system("ticker")
        .ty("Ticker")
        .end()
        .system("dl_counter")
        .ty("DlCounter")
        .from_artifact("counter")
        .end()
        .connect("ticker", "tick_in", "dl_counter", "tick_in")
        .build();
    wiring.artifacts[0].path = Some(lib_path);
    wiring.systems[1].params = ParamSource::Postcard(vec![0xFF, 0xFF, 0xFF]);

    // `resolve` calls `fsw_pack_create` at bind time, so the null state is
    // latched into the slot before any cycle runs.
    let mut coord = resolve(&wiring, &ticker_registry())
        .expect("graph resolves even though fsw_pack_create failed inside it");

    let coord = stellarator::run(|| async move {
        coord.run_for(3).await;
        coord
    });

    // A null-state slot never executes, so nothing later would update its
    // state; `make_slot` latches `Stopped(Panicked)` up front and `stopped()`
    // reports it here.
    let stopped = coord.stopped();
    assert_eq!(
        stopped.len(),
        1,
        "the failed-create dl system is reported stopped"
    );
    // Stopped systems are named type-level, like a static system's
    // `System::NAME`: for a dl system that is the pack entry name.
    assert_eq!(&*stopped[0].name, "DlCounter");
    assert_eq!(stopped[0].reason, StopReason::Panicked);

    drop(coord);
}

/// `type=` selects the pack entry a dl system instantiates. Over a
/// multi-entry pack an omitted `type=` is a clean `PackTypeRequired` error
/// listing the choices, and a `type=` naming no exported entry fails with
/// `PackSystem` wrapping [`DlError::UnknownPackSystem`].
///
/// [`DlError::UnknownPackSystem`]: metor_fsw_2::DlError::UnknownPackSystem
#[test]
fn dl_type_selects_the_pack_entry_and_unknown_type_is_rejected() {
    use metor_fsw_2::DlError;
    use metor_fsw_2::wiring::LoadErrorKind;

    // A dl system with `artifact=` but no `type=`: the builder leaves the type
    // unset, so resolve must pick the pack entry itself.
    let mut wiring = WiringBuilder::new()
        .coordinator_spec(dl_coordinator())
        .artifact(
            "counter",
            "metor-fsw-2-dl-fixture",
            "metor_fsw_2_dl_fixture",
        )
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

    if let Err(e) = provision_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: provision_artifacts failed: {e}");
        return;
    }

    // The fixture pack exports two entries, so the type-less spec cannot pick
    // one; resolve reports the choices instead of guessing.
    let registry = ticker_registry();
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
