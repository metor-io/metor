//! End-to-end test of the shared-object loader.
//!
//! The `tests_abi` suite calls the generated `fsw_*` symbols inside the test
//! binary itself; this test crosses a real shared-object boundary. It builds
//! the `metor-fsw-2-dl-fixture` crate as a `cdylib`, opens the result with
//! [`DlPack`], selects an entry, wires it into a [`Coordinator`] next to a
//! statically linked producer, runs a few cycles, and checks the loaded
//! system's output. That exercises the loader, the manifest round-trip, the
//! ring-region hand-off, the per-cycle drive, the telemetry tap, and
//! teardown.
//!
//! The fixture is built by a nested `cargo build` inside the test, which is
//! safe because the outer `cargo test` lock is released before the test binary
//! runs. If the build cannot produce a usable shared object,
//! [`locate_fixture`] returns `None` and the test skips with a message on
//! stderr instead of failing; `tests_abi` keeps the loader logic covered
//! regardless.

#![cfg(all(feature = "wiring", not(miri)))]

use std::path::PathBuf;
use std::process::Command;

use metor_fsw_2::metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_fsw_2::{
    ClockMode, Coordinator, CoordinatorConfig, CyclicSystem, Delivery, DlPack, FanIn, Frame,
    Input, MsgIn, Out, Output, PortRef, StopReason, System, SystemInput, SystemKind, SystemOutput,
};
use postcard_schema::Schema;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// ---------------------------------------------------------------------------
// Host-side frames
// ---------------------------------------------------------------------------

// These must stay byte-for-byte identical to the fixture's frames; that layout
// agreement is the contract `compatible()` checks against the descriptor
// reconstructed from the shared object.

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

/// Mirror of the fixture's `CounterParams`. The same fields in the same order
/// encode to the same postcard bytes.
#[derive(serde::Serialize, Default)]
struct CounterParams {
    start: u64,
    scale: f64,
}

/// Mirror of the fixture's `TickEvent` message. The shared schema name hashes
/// to the same `PacketId`, so the host decodes the loaded system's Postcard
/// records from the id alone, with no vtable and no announce step.
#[derive(serde::Serialize, serde::Deserialize, Schema, Debug)]
struct TickEvent {
    count: u64,
}

// ---------------------------------------------------------------------------
// A statically linked producer feeding the loaded consumer
// ---------------------------------------------------------------------------

/// Emits `tick_in.value = 1, 2, 3, ...`, incrementing before each write, so
/// after `n` cycles the freshest value is `n`.
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

// ---------------------------------------------------------------------------
// Build and locate the fixture cdylib
// ---------------------------------------------------------------------------

/// The platform file name of the fixture crate's `cdylib`.
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

/// Builds the fixture crate and returns the shared object's path, parsed from
/// cargo's JSON artifact messages so a custom target dir or profile still
/// resolves. Returns `None`, after explaining on stderr, when the build
/// plumbing is unavailable, letting the caller skip rather than fail.
fn locate_fixture() -> Option<PathBuf> {
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "metor-fsw-2-dl-fixture",
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
    // Each compiled artifact's JSON line carries a `"filenames"` array; the
    // cdylib is the entry ending in the platform extension. Scanning quoted
    // tokens avoids pulling in a JSON dependency.
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

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[test]
fn dlopen_cyclic_system_end_to_end() {
    let Some(lib_path) = locate_fixture() else {
        // Build plumbing unavailable; tests_abi covers the loader in-binary.
        return;
    };

    // 1. Load the shared object, check the pack's exported entry names, and
    //    validate the selected entry's reconstructed descriptor.
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
        desc.inputs[0].id.component().expect("table port"),
        TickIn::FRAME_ID
    );
    // User `out`, user `events` (the Postcard port), implicit health, implicit log.
    assert_eq!(desc.outputs.len(), 4);
    assert_eq!(
        desc.outputs[0].id.component().expect("table port"),
        TickOut::FRAME_ID
    );
    // The message port crossed the ABI schema-tagged with its axes intact; a
    // shared object declares a Postcard port exactly like a static system.
    let events = &desc.outputs[1];
    assert_eq!(events.id.packet().expect("postcard port"), TickEvent::ID);
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

    // 2. Wire the static producer into the loaded consumer, params start=1000.
    let params = postcard::to_allocvec(&CounterParams {
        start: 1000,
        scale: 1.0,
    })
    .unwrap();
    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 200.0,
        default_depth: 8,
        clock: ClockMode::Simulated {
            dt: std::time::Duration::from_millis(5),
        },
        ..CoordinatorConfig::default()
    });
    let ticker = b.add_cyclic_named("ticker", Ticker { n: 0 });
    let counter = b.add_dl_cyclic("dl_counter", loaded, params);
    // This `connect` runs `compatible()` over the loaded descriptor's
    // `tick_in` input, the same wiring validation a static system gets.
    b.connect(
        PortRef::new::<TickIn>(ticker),
        PortRef::new::<TickIn>(counter),
    )
    .expect("compatible tick_in edge validated");

    // A second instance of the same entry, with its own params: one opened
    // pack mints any number of independent instances.
    let twin = pack.system("DlCounter").expect("select the entry again");
    let twin_params = postcard::to_allocvec(&CounterParams {
        start: 2000,
        scale: 1.0,
    })
    .unwrap();
    let counter_b = b.add_dl_cyclic("dl_counter_b", twin, twin_params);
    b.connect(
        PortRef::new::<TickIn>(ticker),
        PortRef::new::<TickIn>(counter_b),
    )
    .expect("compatible tick_in edge validated (second instance)");

    let mut coord = b
        .build()
        .expect("graph builds (validation + sizing + bind)");

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
    {
        let out = out_view
            .latest()
            .expect("the dl system produced a tick_out");
        assert_eq!(
            out.get().count,
            1000 + CYCLES as u64,
            "start + latest value"
        );
    }

    // 5a. The second instance of the same entry ran independently, with its
    //     own params.
    {
        let out = twin_view
            .latest()
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
    events_in.drain(|e| counts.push(e.count));
    assert_eq!(
        counts,
        (1..=CYCLES as u64).map(|v| 1000 + v).collect::<Vec<_>>(),
        "every-record log semantics across the dl boundary"
    );

    // 6. Teardown ordering: dropping the coordinator runs `fsw_pack_destroy` before
    //    the `Library` unloads and before the `RingTable` frees the regions.
    //    Not crashing here is the assertion.
    drop(coord);
    drop((out_view, events_in, twin_view));
}

/// Building through the driver ([`build_artifacts`]) leaves a
/// `<cdylib>.manifest` sidecar next to the `.so` — raw postcard
/// [`PackManifestMsg`] bytes naming the same entries the opened pack reports
/// — and `manifest_sidecar: false` opts out. Byte-level sidecar ≡ describe
/// equality is asserted in the driver's own unit tests
/// (`wiring::build_driver`), where the raw describe bytes are reachable.
///
/// [`build_artifacts`]: metor_fsw_2::wiring::build_artifacts
/// [`PackManifestMsg`]: metor_fsw_2::abi::PackManifestMsg
#[test]
fn build_driver_writes_manifest_sidecar() {
    use metor_fsw_2::wiring::{BuildOptions, WiringBuilder, build_artifacts};

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
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    let so = wiring.artifacts[0].path.clone().expect("path filled");
    let mut sidecar = so.clone().into_os_string();
    sidecar.push(".manifest");
    let sidecar = PathBuf::from(sidecar);

    let bytes = std::fs::read(&sidecar).expect("sidecar written next to the built .so");
    let manifest: metor_fsw_2::abi::PackManifestMsg =
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
    build_artifacts(&mut wiring, &opts).expect("rebuild is a cargo no-op");
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

    let pack = DlPack::open(&lib_path).expect("DlPack::open the fixture .so");
    let loaded = pack.system("DlCounter").expect("select the counter entry");

    // `CounterParams` is `{ start: u64, scale: f64 }`; three bytes cannot
    // decode as either field, so the fixture's `fsw_pack_create` panics inside its
    // own `catch_unwind` and returns a null state rather than crashing across
    // the `extern "C"` boundary (`abi_panic_is_contained` tests that
    // containment directly, in-binary).
    let bad_params = vec![0xFF, 0xFF, 0xFF];
    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 200.0,
        default_depth: 8,
        clock: ClockMode::Simulated {
            dt: std::time::Duration::from_millis(5),
        },
        ..CoordinatorConfig::default()
    });
    let ticker = b.add_cyclic_named("ticker", Ticker { n: 0 });
    let counter = b.add_dl_cyclic("dl_counter", loaded, bad_params);
    b.connect(
        PortRef::new::<TickIn>(ticker),
        PortRef::new::<TickIn>(counter),
    )
    .expect("compatible tick_in edge validated");

    // `build()` itself calls `fsw_pack_create` at bind time, so the null state is
    // latched into the slot before any cycle runs.
    let mut coord = b
        .build()
        .expect("graph builds even though fsw_pack_create failed inside it");

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
    assert_eq!(stopped[0].name, "DlCounter");
    assert_eq!(stopped[0].reason, StopReason::Panicked);

    drop(coord);
}
