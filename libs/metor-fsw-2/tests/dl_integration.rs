//! Host-loader acceptance gate: a **real `dlopen`**.
//!
//! Unlike the in-binary `tests_abi` (which calls the generated `fsw_*` symbols in the
//! same crate), this test builds the `metor-fsw-2-dl-fixture` crate as a `cdylib`,
//! `dlopen`s the produced shared object through [`DlSystem`], wires it into a real
//! [`Coordinator`] alongside a statically-linked producer, runs several cycles, and
//! asserts the dlopen'd system produced correct output — proving the loader, the
//! descriptor round-trip, the ring-region hand-off, the per-cycle drive, the telemetry
//! tap, and clean teardown across a genuine `.so` boundary.
//!
//! The fixture build runs **inside** the test (the outer `cargo test` lock is released
//! before the test binary runs, so a nested `cargo build` is safe). If the build
//! plumbing ever proves unreliable on a target, the real-`.so` body is gated behind
//! [`locate_fixture`] returning `None` with a clear skip message, while the always-run
//! `tests_abi` suite keeps covering the loader logic against the in-binary symbols.

#![cfg(all(feature = "kdl", not(miri)))]

use std::path::PathBuf;
use std::process::Command;

use metor_fsw_2::metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_fsw_2::{
    ClockMode, Coordinator, CoordinatorConfig, CyclicSystem, Delivery, DlSystem, FanIn, Frame,
    Input, MsgIn, OnLap, Out, Output, PortRef, StopReason, System, SystemInput, SystemKind,
    SystemOutput,
};
use postcard_schema::Schema;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// ---------------------------------------------------------------------------
// Host-side frames (byte-for-byte the fixture's — the compile-time contract
// `compatible()` enforces over the reconstructed descriptor).
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

/// Mirror of the fixture's `CounterParams` so the host encodes the same postcard bytes
/// (same field set + order — the postcard wire contract).
#[derive(serde::Serialize, Default)]
struct CounterParams {
    start: u64,
    scale: f64,
}

/// Mirror of the fixture's `TickEvent` message: same schema name → same hashed
/// `PacketId`, so the host decodes the dl system's Postcard records with nothing
/// but the id (self-describing — no vtable, no announce).
#[derive(serde::Serialize, serde::Deserialize, Schema, Debug)]
struct TickEvent {
    count: u64,
}

// ---------------------------------------------------------------------------
// A statically-linked producer feeding the dlopen'd consumer.
// ---------------------------------------------------------------------------

/// Emits `tick_in.value = 1, 2, 3, …` (incremented before each write), so after `n`
/// cycles the freshest value is `n`. A statically-linked (host `BoxBacking`) system.
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
// Build + locate the fixture cdylib.
// ---------------------------------------------------------------------------

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

/// `cargo build -p metor-fsw-2-dl-fixture` and return the produced shared object's
/// path, parsed from cargo's JSON artifact messages (robust to a custom target dir /
/// profile). Returns `None` (with an explanation on stderr) if the build plumbing is
/// unavailable, so the caller can skip rather than fail spuriously.
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
    // Each JSON line for a compiled artifact carries a `"filenames":[ ... ]` array; the
    // cdylib is the entry ending in the platform extension. Scan without a JSON dep.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let want = fixture_lib_name();
    for line in stdout.lines() {
        if !line.contains("compiler-artifact") || !line.contains(&want) {
            continue;
        }
        // Pull every quoted token and take the one that is an existing path to `want`.
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
// The gate.
// ---------------------------------------------------------------------------

#[test]
fn dlopen_cyclic_system_end_to_end() {
    let Some(lib_path) = locate_fixture() else {
        // Build plumbing unavailable: skip (tests_abi covers the loader logic in-binary).
        return;
    };

    // 1. Load + validate the descriptor (the existing wiring validation runs over it).
    let loaded = DlSystem::open(&lib_path).expect("DlSystem::open the fixture .so");
    let desc = loaded.descriptor();
    assert_eq!(desc.name, "dl_counter");
    assert_eq!(desc.kind, SystemKind::Cyclic);
    assert_eq!(desc.inputs.len(), 1, "one user input (tick_in)");
    assert_eq!(desc.inputs[0].id.component().expect("table port"), TickIn::FRAME_ID);
    // user `out` + user `events` (the Postcard port) + implicit health + implicit log.
    assert_eq!(desc.outputs.len(), 4);
    assert_eq!(desc.outputs[0].id.component().expect("table port"), TickOut::FRAME_ID);
    // The message port crossed the ABI schema-tagged, axes intact (C7): a `.so`
    // declares a Postcard port exactly like a static system.
    let events = &desc.outputs[1];
    assert_eq!(events.id.packet().expect("postcard port"), TickEvent::ID);
    assert_eq!(events.name, "TickEvent", "the NamedMsg token survives the wire");
    assert_eq!(events.delivery, Delivery::Log);
    assert_eq!(events.fan_in, FanIn::Many);
    assert_eq!(events.on_lap, OnLap::Resync);
    assert!(events.telemetered);
    assert!(desc.capabilities.is_empty(), "a .so can hold no host capabilities");

    // 2. Wire a static producer → the dlopen'd consumer, with params start=1000.
    let params = postcard::to_allocvec(&CounterParams { start: 1000, scale: 1.0 }).unwrap();
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
    // The `connect` runs `compatible()` over the dl descriptor's `tick_in` input —
    // wiring validation against a dlopen'd system, identical to a static one.
    b.connect(PortRef::new::<TickIn>(ticker), PortRef::new::<TickIn>(counter))
        .expect("compatible tick_in edge validated");

    let mut coord = b.build().expect("graph builds (validation + sizing + bind)");

    // 3. Tap the dl system's output via the telemetry registry *before* running (a
    //    fresh view only sees records committed from now on).
    let mut out_view: Input<TickOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("dl_counter.tick_out"))
            .expect("dl output is registered for telemetry `All`")
            .expect("a reader slot is available"),
    );
    // The Postcard port is a registered message channel like any static system's:
    // keyed `<instance>.<NAME>`, drained with an ordinary host-side MsgIn.
    let mut events_in: MsgIn<TickEvent> = MsgIn::new(
        coord
            .registry()
            .view(ComponentId::new("dl_counter.TickEvent"))
            .expect("dl message channel is registered")
            .expect("a reader slot is available"),
    );

    // 4. Run several cycles (producer runs before consumer each cycle, so the consumer
    //    samples this cycle's fresh value). `run_for` does init (fsw_bind_init) → run
    //    (fsw_execute) → shutdown (fsw_shutdown); `coord` is handed back so it stays
    //    alive across the read below.
    const CYCLES: usize = 6;
    let coord = stellarator::run(|| async move {
        coord.run_for(CYCLES).await;
        coord
    });

    // 5. The dlopen'd system produced correct output: count = start(1000) + value(6).
    let out = out_view
        .latest()
        .expect("the dl system produced a tick_out");
    assert_eq!(out.get().count, 1000 + CYCLES as u64, "start + latest value");

    // 5b. The dl system's Postcard port wired end-to-end: every cycle's event is on
    //     the log channel, in order, decoded purely from the self-describing id.
    let mut counts: Vec<u64> = Vec::new();
    events_in.drain(|e| counts.push(e.count));
    assert_eq!(
        counts,
        (1..=CYCLES as u64).map(|v| 1000 + v).collect::<Vec<_>>(),
        "every-record log semantics across the dl boundary"
    );

    // 6. Clean teardown: dropping the coordinator runs `DlSlot::Drop` → `fsw_destroy`
    //    before the `Library` unloads and before the `RingTable` frees the regions.
    //    No crash here is the teardown-ordering assertion.
    drop(coord);
    drop((out_view, events_in));
}

/// R5 (review-findings.md): a `fsw_create` that fails (here, params that don't
/// postcard-decode as `CounterParams`, so `run_create`'s `catch_unwind` returns null
/// per the fixture's real `.so`) must not leave the slot looking permanently
/// `Running`. It must surface through the coordinator's stopped-systems telemetry
/// from the very first cycle, exactly like a lapped input or an in-cycle panic would.
#[test]
fn dlopen_null_create_reports_stopped() {
    let Some(lib_path) = locate_fixture() else {
        // Build plumbing unavailable: skip (tests_abi covers the loader logic in-binary).
        return;
    };

    let loaded = DlSystem::open(&lib_path).expect("DlSystem::open the fixture .so");

    // Deliberately malformed: `CounterParams` is `{ start: u64, scale: f64 }`; three
    // bytes cannot postcard-decode as either field, so the fixture's `fsw_create`
    // panics inside its own `catch_unwind` and returns a null state (never a crash
    // across the `extern "C"` boundary — see `abi_panic_is_contained` for the direct
    // in-binary version of that containment guarantee).
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
    b.connect(PortRef::new::<TickIn>(ticker), PortRef::new::<TickIn>(counter))
        .expect("compatible tick_in edge validated");

    // `build()` itself calls `fsw_create` (`into_slot`, at bind time) — the null state
    // is latched into the slot right there, before any cycle runs.
    let mut coord = b.build().expect("graph builds even though fsw_create failed inside it");

    let coord = stellarator::run(|| async move {
        coord.run_for(3).await;
        coord
    });

    // The zombie-slot bug (pre-fix): a null-state `DlSlot::step` early-returns without
    // touching `slot_state`, which defaulted to `Running`, so `stopped()` would stay
    // empty forever. Fixed: `make_slot` latches `Stopped(Panicked)` up front.
    let stopped = coord.stopped();
    assert_eq!(stopped.len(), 1, "the failed-create dl system is reported stopped");
    assert_eq!(stopped[0].name, "dl_counter");
    assert_eq!(stopped[0].reason, StopReason::Panicked);

    drop(coord);
}
