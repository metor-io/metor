//! Drive the real `seq-fixture` pack, compiled to wasm, through the whole ABI
//! lifecycle — and prove the two properties the substrate exists for: a poll
//! is bounded by fuel, and a fault is contained.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use metor_fsw_2_core::abi::{FswRing, FswStatus, ROLE_INPUT, ROLE_OUTPUT};
use metor_fsw_2_core::{Delivery, SequenceStatus, SlotControlIn, Timestamp, capacity_for};
use metor_fsw_ring::{Config, NoWake, RingBuffer};

use super::*;

/// A budget far above what the fixture's poll needs, so the lifecycle tests
/// are not accidentally measuring the fuel policy.
const AMPLE_FUEL: u64 = 100_000_000;

/// The fixture module, built on demand.
///
/// Building it here rather than in a `build.rs` keeps the wasm toolchain off
/// the critical path of an ordinary `cargo build`: only these tests need the
/// `wasm32-unknown-unknown` target installed, and they say so plainly when it
/// is missing rather than failing somewhere obscure.
fn fixture() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let out = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "metor-fsw-2-seq-fixture",
                "--target",
                "wasm32-unknown-unknown",
                "--release",
            ])
            .current_dir(workspace_root())
            .output()
            .expect("running cargo to build the wasm fixture");
        assert!(
            out.status.success(),
            "building the wasm fixture failed (is the wasm32-unknown-unknown \
             target installed? `rustup target add wasm32-unknown-unknown`):\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let path = workspace_root()
            .join("target/wasm32-unknown-unknown/release/metor_fsw_2_seq_fixture.wasm");
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    })
}

/// `libs/metor-fsw-2` → the workspace root.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// The module loads, agrees with the host's ABI word, and its manifest decodes
/// to the same entries the `.so` path sees.
#[test]
fn wasm_pack_loads_and_describes() {
    let mut pack = WasmPack::open(fixture(), AMPLE_FUEL).expect("loads");
    let names: Vec<_> = pack
        .manifest()
        .systems
        .iter()
        .map(|s| s.descriptor.name.to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "waiter"),
        "the manifest names the fixture's entries, got {names:?}"
    );
    pack.close().expect("closes");
}

/// The full lifecycle: create the `waiter` entry as a slot occupant, bind its
/// ports to rings living inside guest memory, and cycle it until it finishes.
///
/// `waiter` declares no user ports, so its contract is the mount-appended
/// slot-control input plus the status, health, and log outputs every occupant
/// carries.
///
/// Two things had to be fixed on the guest side before this could pass, both
/// the same shape: `wasm32-unknown-unknown` has no operating system, and the
/// ring and driver reached for one. Claiming a ring slot stamped
/// `std::process::id`, and every execute timed itself with `Instant::now`;
/// both are unsupported on that target and panic, which surfaces only as an
/// opaque trap because the module imports nothing and the target aborts
/// rather than unwinds, so `catch_unwind` cannot turn the panic into a status
/// word. Neither remains: the host times each execute itself.
#[test]
fn wasm_occupant_runs_to_a_terminal_state() {
    let mut pack = WasmPack::open(fixture(), AMPLE_FUEL).expect("loads");
    let index = entry(&pack, "waiter");
    let desc = pack.manifest().systems[index as usize].descriptor.clone();

    // Mount 1 is the slot-occupant mount: it appends the control input and the
    // status output to whatever the entry declares.
    let state = pack.create(index, 1, &[]).expect("creates");

    // Every ring must hold `depth` records of its port's largest frame, so
    // each is sized from the port rather than given a flat capacity.
    let cfg_for = |max_size: usize| Config {
        capacity: capacity_for(max_size, 8),
        max_readers: 2,
    };

    // The occupant mount appends a `SlotControlIn` input and a
    // `SequenceStatus` output the entry never declares, so the bound contract
    // is one longer than the descriptor on each side.
    let mut inputs = Vec::new();
    for port in &desc.inputs {
        inputs.push(
            pack.add_ring(cfg_for(port.max_size), ROLE_INPUT)
                .expect("input ring"),
        );
    }
    inputs.push(
        pack.add_ring(cfg_for(size_of::<SlotControlIn>()), ROLE_INPUT)
            .expect("slot control ring"),
    );

    let mut outputs = Vec::new();
    for port in &desc.outputs {
        outputs.push(
            pack.add_ring(cfg_for(port.max_size), ROLE_OUTPUT)
                .expect("output ring"),
        );
    }
    outputs.push(
        pack.add_ring(cfg_for(size_of::<SequenceStatus>()), ROLE_OUTPUT)
            .expect("status ring"),
    );
    pack.bind_init(state, &inputs, &outputs, "waiter")
        .expect("binds");

    // The body waits two simulated microseconds, so stepping the clock a
    // microsecond per cycle finishes quickly; the loop bound stops a broken
    // guest hanging the suite.
    let mut status = FswStatus::Running;
    for cycle in 0..64u64 {
        status = pack.execute(state, cycle).expect("no trap");
        if status != FswStatus::Running {
            break;
        }
    }
    assert_eq!(
        status,
        FswStatus::Done,
        "the waiter reaches a terminal state"
    );

    pack.shutdown(state).expect("shuts down");
    pack.destroy(state).expect("destroys");
    pack.close().expect("closes");
}

/// **The property no natively linked occupant can offer.** A budget too small
/// for the work cuts the guest off mid-instruction: the host survives and gets
/// an error value, where a `.so` in the same situation would simply never
/// return and would stall the cycle.
#[test]
fn a_miserly_fuel_budget_stops_the_guest() {
    let Err(err) = WasmPack::open(fixture(), 1_000) else {
        panic!("1k fuel should not reach the end of open")
    };
    assert!(
        is_out_of_fuel(&err) || matches!(err, WasmError::Instantiate(_)),
        "expected the budget to bite, got {err:?}"
    );

    // The same module under an ample budget gets through, so the failure above
    // is the policy biting rather than a broken artifact.
    let mut ok = WasmPack::open(fixture(), AMPLE_FUEL).expect("ample fuel loads");
    ok.close().expect("closes");
}

/// **The other property.** A guest reaching outside its linear memory traps,
/// and the host is untouched — the same access in a `.so` would corrupt the
/// host's memory silently.
#[test]
fn an_out_of_bounds_guest_traps_without_touching_the_host() {
    // One page of memory, and an export that stores far past the end of it.
    let wasm = wat::parse_str(
        r#"
        (module
          (memory 1)
          (func (export "boom")
            i32.const 1000000
            i32.const 42
            i32.store))
        "#,
    )
    .expect("assembles");

    let mut config = WasmConfig::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, &wasm[..]).expect("valid module");
    let mut store = Store::new(&engine, ());
    store.set_fuel(AMPLE_FUEL).expect("fuel");
    let linker: Linker<()> = Linker::new(&engine);
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .expect("instantiates");
    let boom: TypedFunc<(), ()> = instance.get_typed_func(&store, "boom").expect("export");

    assert!(
        boom.call(&mut store, ()).is_err(),
        "an out-of-bounds store traps the guest"
    );
    // The host is fine: its store is still usable after the guest faulted.
    assert!(store.get_fuel().is_ok(), "the host survives the trap");
}

/// The manifest index of the entry named `name`.
fn entry(pack: &WasmPack, name: &str) -> u32 {
    pack.manifest()
        .systems
        .iter()
        .position(|s| s.descriptor.name == name)
        .unwrap_or_else(|| panic!("no `{name}` entry")) as u32
}

fn waiter_identity() -> Vec<u8> {
    let pack = WasmPack::open(fixture(), AMPLE_FUEL).expect("loads");
    let entry = pack
        .manifest()
        .systems
        .iter()
        .find(|entry| entry.descriptor.name == "waiter")
        .expect("waiter entry");
    entry_identity(entry)
}

/// The guard that makes holding a handle over guest memory sound at all.
///
/// A host handle into the interpreter's backing buffer dangles the moment the
/// guest grows its memory, so the host records the size once the regions exist
/// and refuses to trust a handle after that changes. Allocating enough to
/// force growth must be *noticed*, not silently tolerated — the alternative to
/// noticing is reading freed memory.
#[test]
fn guest_memory_is_frozen_after_handles_are_pinned() {
    let mut pack = WasmPack::open(fixture(), AMPLE_FUEL).expect("loads");
    pack.pin_memory();
    pack.check_memory_stable()
        .expect("stable before any growth");

    // Ask the guest allocator for enough bytes to require memory.grow. The
    // store limiter must trap that request rather than moving the backing
    // allocation out from under retained bridge handles.
    let before = pack.memory_len();
    let mut refused = None;
    for _ in 0..64 {
        if let Err(err) = pack.alloc_for_test(1 << 20) {
            refused = Some(err);
            break;
        }
    }
    assert!(
        matches!(refused, Some(WasmError::Trap(_))),
        "post-bind growth must trap, got {refused:?}"
    );
    assert_eq!(pack.memory_len(), before, "linear memory never moved");
    pack.check_memory_stable().expect("handles remain valid");
}

#[test]
fn initial_memory_over_the_host_limit_is_rejected() {
    let wasm = wat::parse_str("(module (memory (export \"memory\") 2))").expect("valid module");
    let err = WasmPack::open_with_memory_limit(&wasm, AMPLE_FUEL, 64 * 1024)
        .err()
        .expect("two pages exceed a one-page host limit");
    assert!(matches!(err, WasmError::Instantiate(_)), "got {err:?}");
}

/// The bridge carries records both ways, and — the part that motivated holding
/// handles at all — it carries a *backlog*, not just the newest record.
///
/// A view re-attached each cycle would rejoin at the live edge and lose
/// everything written since the last pump, so this writes several records
/// before pumping and insists all of them arrive, in order.
#[test]
fn the_bridge_carries_a_backlog_both_ways() {
    let mut pack = WasmPack::open(fixture(), AMPLE_FUEL).expect("loads");
    let cfg = Config {
        capacity: capacity_for(64, 16),
        max_readers: 4,
    };

    // One guest region each way, and a coordinator-owned host ring each way.
    let g_in = pack.add_ring(cfg, ROLE_INPUT).expect("guest input");
    let g_out = pack.add_ring(cfg, ROLE_OUTPUT).expect("guest output");
    let h_in = RingBuffer::create_in_memory(cfg);
    let h_out = RingBuffer::create_in_memory(cfg);
    pack.pin_memory();

    let base = pack.memory_base();

    // Register the guest-side reader *before* the first pump: a slot joins at
    // the live edge, so a view taken afterwards would see nothing. This is the
    // same ordering the real occupant gets, since it binds before any cycle.
    // SAFETY: the guest region is live, and `pin_memory`/`check_memory_stable`
    // bracket every use below; nothing grows the guest in between.
    let g_in_ring = unsafe { RingBuffer::attach_raw(base.add(g_in.offset as usize), g_in.len) }
        .expect("attach");
    let mut guest_reader = g_in_ring.view(NoWake).expect("reader slot");

    // SAFETY: as above, for every region the bridge attaches.
    let mut bridge = unsafe {
        RingBridge::new(
            base,
            &[h_in.region()],
            &[g_in],
            &[Delivery::Log],
            &[h_out.region()],
            &[g_out],
            &[Delivery::Log],
        )
    }
    .expect("bridge builds");

    // A producer writes three records before the first pump: the backlog case.
    let mut producer = h_in.writer(NoWake).expect("sole host writer");
    for i in 0..3u8 {
        producer.try_write(&[i; 8]).expect("host ring takes it");
    }

    pack.check_memory_stable().expect("guest has not moved");
    bridge.pump_in().expect("input pump");

    // All three crossed inbound, in order — not just the newest.
    for expect in 0..3u8 {
        let got = guest_reader
            .try_read()
            .expect("readable")
            .expect("a forwarded record");
        assert_eq!(&got[..], &[expect; 8], "inbound records arrive in order");
    }
    assert!(
        guest_reader.try_read().expect("readable").is_none(),
        "exactly the three written records crossed inbound"
    );

    // Now the reverse leg: a guest-side producer, pumped out to the host.
    // SAFETY: same live, pinned region.
    let g_out_ring = unsafe { RingBuffer::attach_raw(base.add(g_out.offset as usize), g_out.len) }
        .expect("attach");
    let mut host_reader = h_out.view(NoWake).expect("reader slot");
    let mut guest_producer = g_out_ring.writer(NoWake).expect("sole guest writer");
    for i in 10..13u8 {
        guest_producer
            .try_write(&[i; 8])
            .expect("guest ring takes it");
    }

    pack.check_memory_stable().expect("guest has not moved");
    bridge.pump_out().expect("output pump");

    for expect in 10..13u8 {
        let got = host_reader
            .try_read()
            .expect("readable")
            .expect("a forwarded record");
        assert_eq!(&got[..], &[expect; 8], "records arrive in order");
    }
    assert!(
        host_reader.try_read().expect("readable").is_none(),
        "exactly the three written records crossed"
    );
    assert_eq!(bridge.dropped(), 0, "nothing was dropped");
}

/// A snapshot leg forwards the newest record and nothing else, and — the part
/// worth pinning — forwards it *once*.
///
/// `try_latest` re-serves the pinned newest record when nothing new has
/// arrived, so a snapshot leg without the `last_committed` skip would re-send
/// the same record every cycle: the consumer would see a stream of duplicates
/// and, on a shallow ring, real records would be dropped to make room for
/// them. The shared snapshot ring pump carries this guard for the same reason.
#[test]
fn a_snapshot_leg_forwards_the_newest_once() {
    let mut pack = WasmPack::open(fixture(), AMPLE_FUEL).expect("loads");
    let cfg = Config {
        capacity: capacity_for(64, 16),
        max_readers: 4,
    };
    let g_in = pack.add_ring(cfg, ROLE_INPUT).expect("guest input");
    let h_in = RingBuffer::create_in_memory(cfg);
    pack.pin_memory();
    let base = pack.memory_base();

    // SAFETY: live, pinned regions; `check_memory_stable` guards every use.
    let g_ring = unsafe { RingBuffer::attach_raw(base.add(g_in.offset as usize), g_in.len) }
        .expect("attach");
    let mut guest_reader = g_ring.view(NoWake).expect("reader slot");
    // SAFETY: as above.
    let mut bridge = unsafe {
        RingBridge::new(
            base,
            &[h_in.region()],
            &[g_in],
            &[Delivery::Snapshot],
            &[],
            &[],
            &[],
        )
    }
    .expect("bridge builds");

    // Three records upstream, but a snapshot consumer wants only the newest.
    let mut producer = h_in.writer(NoWake).expect("sole host writer");
    for i in 0..3u8 {
        producer.try_write(&[i; 8]).expect("host ring takes it");
    }
    bridge.pump_in().expect("first snapshot pump");

    assert_eq!(
        &guest_reader
            .try_read()
            .expect("readable")
            .expect("the newest record")[..],
        &[2u8; 8],
        "a snapshot leg collapses the backlog to the newest record"
    );
    assert!(
        guest_reader.try_read().expect("readable").is_none(),
        "and forwards only that one"
    );

    // Pump repeatedly with nothing new upstream: the pinned record must not be
    // forwarded again.
    for _ in 0..5 {
        bridge.pump_in().expect("idle snapshot pump");
    }
    assert!(
        guest_reader.try_read().expect("readable").is_none(),
        "an idle snapshot leg re-forwards nothing"
    );

    // A genuinely new record still crosses.
    producer.try_write(&[9u8; 8]).expect("host ring takes it");
    bridge.pump_in().expect("new snapshot pump");
    assert_eq!(
        &guest_reader
            .try_read()
            .expect("readable")
            .expect("the new record")[..],
        &[9u8; 8],
        "a new commit still crosses"
    );
    assert_eq!(bridge.dropped(), 0, "nothing was dropped");
}

/// A slot's ring templates for the `waiter` contract: one control input, and
/// log and status outputs. Returns the rings (which must outlive the
/// occupant) alongside the `FswRing` handles a slot hands its occupant.
fn slot_rings() -> (Vec<RingBuffer>, Vec<FswRing>, Vec<FswRing>) {
    let make = |max_size: usize, role: u8| {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: capacity_for(max_size, 8),
            max_readers: 4,
        });
        let (base, len) = ring.region();
        (ring, FswRing { base, len, role })
    };
    // Occupant-mount order: inputs [control]; outputs [log, status].
    let (c, ch) = make(size_of::<SlotControlIn>(), ROLE_INPUT);
    let (l, lh) = make(4096, ROLE_OUTPUT);
    let (s, sh) = make(size_of::<SequenceStatus>(), ROLE_OUTPUT);
    (vec![c, l, s], vec![ch], vec![lh, sh])
}

/// The whole point of Stage B: a wasm occupant bound to a *slot's* rings,
/// cycled by the slot's driver, reaching a terminal state — with its status
/// arriving on the host side of the bridge rather than staying in the guest.
#[test]
fn a_wasm_occupant_drives_a_slot_to_done() {
    let (rings, host_in, host_out) = slot_rings();
    // Tap the status ring before the run: a reader joins at the live edge, so
    // one opened afterwards would see nothing.
    let mut status_tap = rings[2].view(NoWake).expect("status reader slot");

    let pack = WasmPack::open(fixture(), AMPLE_FUEL).expect("loads");
    let entry = entry(&pack, "waiter");
    drop(pack);

    let mut slot = WasmSlot::bind(
        fixture(),
        entry,
        &[],
        "inst",
        &host_in,
        &host_out,
        AMPLE_FUEL,
    )
    .expect("binds to the slot's rings");

    // The body waits two simulated microseconds.
    let mut last = FswStatus::Running;
    for cycle in 0..64u64 {
        last = slot.execute_raw(Timestamp(cycle as i64));
        if last != FswStatus::Running {
            break;
        }
    }
    assert_eq!(
        last,
        FswStatus::Done,
        "the occupant reaches a terminal state"
    );
    assert_eq!(slot.dropped(), 0, "the bridge delivered everything");

    // The status the guest wrote crossed the bridge onto the slot's own ring,
    // which is what makes an occupant observable to the rest of the target.
    assert!(
        status_tap.try_latest().expect("readable").is_some(),
        "a SequenceStatus record reached the host side of the bridge"
    );
}

#[test]
fn dropping_a_wasm_slot_releases_host_ring_roles_before_memory() {
    let (_rings, host_in, host_out) = slot_rings();
    let pack = WasmPack::open(fixture(), AMPLE_FUEL).expect("loads");
    let entry = entry(&pack, "waiter");
    drop(pack);

    for _ in 0..3 {
        let slot = WasmSlot::bind(
            fixture(),
            entry,
            &[],
            "inst",
            &host_in,
            &host_out,
            AMPLE_FUEL,
        )
        .expect("the previous bridge released every host claim");
        drop(slot);
    }
}

#[test]
fn wasm_bridge_drops_are_drained_once() {
    let mut pack = WasmPack::open(fixture(), AMPLE_FUEL).expect("loads");
    let cfg = Config {
        capacity: 16,
        max_readers: 2,
    };
    let guest = pack.add_ring(cfg, ROLE_OUTPUT).expect("guest output");
    pack.pin_memory();
    let base = pack.memory_base();

    let host = RingBuffer::create_in_memory(cfg);
    let _blocked = host.view(NoWake).expect("host reader");
    let (host_base, host_len) = host.region();
    let mut bridge = unsafe {
        RingBridge::new(
            base,
            &[],
            &[],
            &[],
            &[(host_base, host_len)],
            &[guest],
            &[Delivery::Log],
        )
    }
    .expect("bridge");
    let guest_ring = unsafe { RingBuffer::attach_raw(base.add(guest.offset as usize), guest.len) }
        .expect("guest ring");
    let mut guest_writer = guest_ring.writer(NoWake).expect("guest writer");

    guest_writer.try_write(&[1; 8]).expect("first record");
    bridge.pump_out().expect("first pump");
    guest_writer.try_write(&[2; 8]).expect("second record");
    bridge.pump_out().expect("second pump");
    assert_eq!(bridge.drain_dropped(), 1);
    assert_eq!(bridge.drain_dropped(), 0);
}

#[test]
fn compatible_hot_reload_accepts_changed_module_bytes() {
    let (_rings, host_in, host_out) = slot_rings();
    let identity = waiter_identity();

    // Append a valid custom section. The code bytes change while the pack ABI
    // and selected entry manifest remain identical.
    let mut changed = fixture().to_vec();
    changed.extend_from_slice(&[0, 3, 1, b'x', 1]);
    let slot = WasmSlot::bind_compatible(
        &changed,
        "waiter",
        &identity,
        &[],
        "inst",
        &host_in,
        &host_out,
        AMPLE_FUEL,
        AMPLE_FUEL,
        DEFAULT_MAX_MEMORY_BYTES,
    )
    .expect("interface-compatible bytes reload");
    drop(slot);
}

#[test]
fn incompatible_hot_reload_is_rejected_before_binding() {
    let mut identity = waiter_identity();
    identity[0] ^= 1;

    let err = WasmSlot::bind_compatible(
        fixture(),
        "waiter",
        &identity,
        &[],
        "inst",
        &[],
        &[],
        AMPLE_FUEL,
        AMPLE_FUEL,
        DEFAULT_MAX_MEMORY_BYTES,
    )
    .err()
    .expect("changed entry identity is rejected");
    assert!(matches!(err, WasmError::EntryChanged(name) if name == "waiter"));
}

/// A runaway occupant is stopped by its fuel budget and reported as
/// `Panicked` — the status the runner already maps to `SlotState::Stopped` for
/// a `.so` panic, so no wasm-specific handling is needed downstream.
///
/// This is the property a natively linked occupant cannot offer at all: the
/// same runaway in a `.so` never returns and stalls the cycle.
#[test]
fn a_starved_wasm_occupant_stops_instead_of_stalling() {
    let (_rings, host_in, host_out) = slot_rings();
    // Bind generously — binding costs far more fuel than a cycle — then apply
    // a per-poll budget too small to finish one.
    let mut slot = WasmSlot::bind(fixture(), 0, &[], "inst", &host_in, &host_out, AMPLE_FUEL)
        .expect("binds under an ample budget");
    slot.set_fuel_per_call(500);
    assert_eq!(
        slot.execute_raw(Timestamp(0)),
        FswStatus::Panicked,
        "an exhausted budget is terminal, not a stall"
    );
    assert_eq!(
        slot.execute_raw(Timestamp(1)),
        FswStatus::Panicked,
        "and it is latched — a dead occupant is never re-entered"
    );
}

/// A target can *declare* a wasm occupant, and `resolve` describes it without
/// ever loading it into this process.
///
/// This is the step between "the runtime works" and "a target can use it":
/// until resolve understands a wasm artifact, `OccupantBacking::Wasm` is
/// reachable only from inside the crate.
#[test]
fn resolve_accepts_a_wasm_occupant_declared_in_a_target() {
    use crate::wiring::{Registry, resolve};
    use crate::{ClockSpec, WiringBuilder};

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("seq.wasm");
    std::fs::write(&path, fixture()).expect("write module");

    let wiring = WiringBuilder::new()
        .coordinator(1000.0, ClockSpec::Simulated { dt_secs: 0.000002 })
        .wasm_artifact("seqs", &path)
        .slot("mode")
        .allow_from("waiter", "seqs")
        .allow_from("napper", "seqs")
        .end()
        .build();

    let coord = resolve(&wiring, &Registry::with_builtins());
    let coord = coord.unwrap_or_else(|e| panic!("a wasm-backed slot resolves: {e:?}"));
    drop(coord);
}

/// A wasm allow line naming an entry the module does not export fails at
/// build time with a diagnostic that names it, rather than at the first
/// `Load` in flight.
#[test]
fn resolve_rejects_an_unknown_wasm_entry() {
    use crate::wiring::{Registry, resolve};
    use crate::{ClockSpec, WiringBuilder};

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("seq.wasm");
    std::fs::write(&path, fixture()).expect("write module");

    let wiring = WiringBuilder::new()
        .coordinator(1000.0, ClockSpec::Simulated { dt_secs: 0.000002 })
        .wasm_artifact("seqs", &path)
        .slot("mode")
        .allow_from("nope", "seqs")
        .end()
        .build();

    let err = resolve(&wiring, &Registry::with_builtins())
        .err()
        .expect("an entry the module does not export is a build error");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("nope"),
        "the diagnostic names the missing entry, got: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// Compiled Python packs: the pack backend meets the wasm host it was built
// for. The compiler-side facts (manifest shape, determinism, gate rules) are
// pinned in metor-expr's own suite; here the artifact walks the real
// open → create → ring → bind → execute sequence and the run rule shows in
// what lands on the rings.
// ---------------------------------------------------------------------------

/// The build-time resolver a provision pass would derive, reduced to one
/// producer: instance `imu`, port `sensors`, a 3-vector at offset 8 behind
/// the record timestamp.
struct ImuResolver;

const GYRO_PATH: &str = "imu.sensors.gyro_b";
const TEMP_PATH: &str = "imu.sensors.temp";

fn imu_source(path: &str) -> Option<metor_expr::ComponentSource> {
    use metor_proto::types::{ComponentId, PrimType};
    let (name, prim, shape, offset): (_, _, &[usize], _) = match path {
        GYRO_PATH => ("sensors.gyro_b", PrimType::F64, &[3], 8),
        // A narrower element type, so the differential also exercises the
        // guest's f32 → slot conversion.
        TEMP_PATH => ("sensors.temp", PrimType::F32, &[], 32),
        _ => return None,
    };
    Some(metor_expr::ComponentSource {
        instance: "imu".into(),
        port_name: "sensors".into(),
        frame_id: ComponentId::new("sensors"),
        max_size: 36,
        component_id: ComponentId::new(name),
        component_name: name.into(),
        prim,
        shape: shape.to_vec(),
        offset,
    })
}

impl metor_expr::Resolver for ImuResolver {
    fn component(&self, path: &str) -> Option<metor_expr::CompSchema> {
        let source = imu_source(path)?;
        let ty = match source.shape.as_slice() {
            [] => metor_expr::Ty::F64,
            shape => metor_expr::Ty::Tensor {
                dtype: metor_expr::Dtype::F64,
                shape: shape.to_vec(),
            },
        };
        Some(metor_expr::CompSchema { ty })
    }

    fn suffix(&self, name: &str) -> Vec<String> {
        [GYRO_PATH, TEMP_PATH]
            .into_iter()
            .filter(|p| p.ends_with(&format!(".{name}")))
            .map(str::to_string)
            .collect()
    }

    fn frame(&self, _name: &str) -> Option<metor_expr::FrameSchema> {
        None
    }
}

impl metor_expr::PackResolver for ImuResolver {
    fn component_source(&self, path: &str) -> Option<metor_expr::ComponentSource> {
        imu_source(path)
    }
}

fn python_pack() -> metor_expr::PackProgram {
    let source = "@system(\"imu.sensors.gyro_b\")\n\
                  def gyro_norm(gyro_b) -> f64:\n    return (gyro_b @ gyro_b) ** 0.5\n";
    metor_expr::compile_pack(source, &ImuResolver, 120.0).expect("the fixture program compiles")
}

/// The whole pack lifecycle over a compiled Python system, with the run rule
/// observed on real rings: a never-published input skips, a fresh driving
/// record fires and publishes `[now][value]`, a quiet cycle publishes
/// nothing.
#[test]
fn a_compiled_python_system_runs_through_the_pack_host() {
    let program = python_pack();
    let mut pack =
        WasmPack::open(&program.wasm, AMPLE_FUEL).expect("a compiled pack opens under the host");

    // The host's decoded view is the compiler's baked bytes.
    let decoded: metor_fsw_2_core::abi::PackManifest =
        postcard::from_bytes(&program.pack_manifest).expect("baked manifest decodes");
    assert_eq!(pack.manifest().systems.len(), decoded.systems.len());
    assert_eq!(pack.manifest().systems[0].descriptor.name, "gyro_norm");

    let state = pack.create(0, 0, &[]).expect("create entry 0 wired");
    let in_cfg = Config {
        capacity: capacity_for(32, 8),
        max_readers: 2,
    };
    let out_cfg = Config {
        capacity: capacity_for(16, 8),
        max_readers: 2,
    };
    let input = pack.add_ring(in_cfg, ROLE_INPUT).expect("input ring");
    let output = pack.add_ring(out_cfg, ROLE_OUTPUT).expect("output ring");
    pack.bind_init(state, &[input], &[output], "gyro_norm")
        .expect("bind");
    pack.pin_memory();

    let base = pack.memory_base();
    // SAFETY: both regions were just formatted inside pinned guest memory,
    // which outlives the handles below.
    let in_ring = unsafe { RingBuffer::attach_raw(base.add(input.offset as usize), input.len) }
        .expect("host attaches the guest input ring");
    let mut writer = in_ring.writer(NoWake).expect("host writer");
    // SAFETY: as above.
    let out_ring = unsafe { RingBuffer::attach_raw(base.add(output.offset as usize), output.len) }
        .expect("host attaches the guest output ring");
    let mut view = out_ring.view(NoWake).expect("host view");

    // Never-published driving input: the cycle is skipped, not an error.
    assert_eq!(pack.execute(state, 1).unwrap(), FswStatus::Running);
    assert!(
        view.try_latest().expect("output ring intact").is_none(),
        "a skipped cycle publishes nothing"
    );

    // One driving record: [ts][1, 2, 3].
    let mut record = Vec::new();
    record.extend_from_slice(&41i64.to_le_bytes());
    for v in [1.0f64, 2.0, 3.0] {
        record.extend_from_slice(&v.to_le_bytes());
    }
    writer.try_write(&record).expect("input record");
    assert_eq!(pack.execute(state, 42).unwrap(), FswStatus::Running);
    {
        let grant = view
            .try_latest()
            .expect("output ring intact")
            .expect("a fresh driving record fires the system");
        assert_eq!(grant.len(), 16, "[timestamp][f64 norm]");
        let ts = i64::from_le_bytes(grant[0..8].try_into().unwrap());
        let norm = f64::from_le_bytes(grant[8..16].try_into().unwrap());
        assert_eq!(ts, 42, "records are stamped with execute's own now");
        assert_eq!(norm, 14.0f64.sqrt());
    }

    // No new input: execute keeps running but publishes nothing new.
    let committed = out_ring.committed();
    assert_eq!(pack.execute(state, 43).unwrap(), FswStatus::Running);
    assert_eq!(
        out_ring.committed(),
        committed,
        "a quiet driving ring skips the cycle"
    );

    // A second concurrent create of the same entry would alias the static
    // buffers; the guest refuses it like a failed native create.
    assert!(matches!(pack.create(0, 0, &[]), Err(WasmError::Create(0))));

    pack.shutdown(state).expect("shutdown");
    pack.destroy(state).expect("destroy");
    pack.close().expect("close");
}

/// The bit-parity differential (plan WP5): one module, two hosts. The panel
/// drives the expr export family — write the argument frames, call
/// `<system>_eval`, read `<system>_ret_ptr` — while the vehicle feeds
/// producer records through the pack ABI's rings; the output frame bytes
/// must be identical to the bit. A multi-field frame both ways (a tensor
/// beside a scalar, an `f32` source component), since the panel's own use
/// never exercises more than one field.
#[test]
fn the_expr_and_pack_hosts_produce_identical_frames() {
    let source = "\
class Est(Frame):
    norm: f64
    doubled: Tensor[f64, 3]

@system(\"imu.sensors.gyro_b\", \"imu.sensors.temp\")
def est(gyro_b, temp) -> Est:
    return Est(norm=(gyro_b @ gyro_b) ** 0.5 + temp, doubled=gyro_b * 2.0)
";
    let program = metor_expr::compile_pack(source, &ImuResolver, 120.0).expect("compiles");
    const FRAME_BYTES: usize = 8 + 24; // norm, then doubled

    // --- the vehicle host: records in, records out --------------------------
    let mut pack = WasmPack::open(&program.wasm, AMPLE_FUEL).expect("opens");
    let state = pack.create(0, 0, &[]).expect("create");
    let cfg = Config {
        capacity: capacity_for(64, 8),
        max_readers: 2,
    };
    let input = pack.add_ring(cfg, ROLE_INPUT).expect("input ring");
    let output = pack.add_ring(cfg, ROLE_OUTPUT).expect("output ring");
    pack.bind_init(state, &[input], &[output], "est")
        .expect("bind");
    pack.pin_memory();
    let base = pack.memory_base();
    // SAFETY: both regions sit inside pinned guest memory for the test's life.
    let in_ring = unsafe { RingBuffer::attach_raw(base.add(input.offset as usize), input.len) }
        .expect("attach input");
    let mut writer = in_ring.writer(NoWake).expect("writer");
    let out_ring = unsafe { RingBuffer::attach_raw(base.add(output.offset as usize), output.len) }
        .expect("attach output");
    let mut view = out_ring.view(NoWake).expect("view");

    // --- the panel host: argument frames in, the return frame out -----------
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, &program.wasm[..]).expect("validates");
    let mut store = wasmi::Store::new(&engine, ());
    let instance = wasmi::Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .expect("instantiates");
    let memory = instance.get_memory(&store, "memory").expect("memory");
    let accessor = |store: &mut wasmi::Store<()>, name: &str, arg: i32| -> usize {
        let f: wasmi::TypedFunc<i32, i32> = instance.get_typed_func(&*store, name).expect(name);
        f.call(store, arg).expect(name) as usize
    };
    let gyro_at = accessor(&mut store, "est_arg_ptr", 0);
    let temp_at = accessor(&mut store, "est_arg_ptr", 1);
    let ret: wasmi::TypedFunc<(), i32> = instance.get_typed_func(&store, "est_ret_ptr").unwrap();
    let ret_at = ret.call(&mut store, ()).expect("ret_ptr") as usize;
    let eval: wasmi::TypedFunc<i64, i32> = instance.get_typed_func(&store, "est_eval").unwrap();

    for (cycle, (gyro, temp)) in [
        ([0.1f64, -2.5, 3.75], 20.5f32),
        ([1.0, 2.0, 3.0], -4.25),
        ([-0.001, 0.0, 12.5], 0.0),
    ]
    .into_iter()
    .enumerate()
    {
        let now = cycle as i64 + 1;
        // Vehicle: one producer record, [ts][gyro f64 ×3][temp f32].
        let mut record = Vec::new();
        record.extend_from_slice(&now.to_le_bytes());
        for v in gyro {
            record.extend_from_slice(&v.to_le_bytes());
        }
        record.extend_from_slice(&temp.to_le_bytes());
        writer.try_write(&record).expect("input record");
        assert_eq!(pack.execute(state, now as u64).unwrap(), FswStatus::Running);
        let grant = view.try_latest().expect("intact").expect("published");
        assert_eq!(grant.len(), 8 + FRAME_BYTES);
        let vehicle = grant[8..].to_vec();
        drop(grant);

        // Panel: the same values through the argument frames.
        let mut gyro_bytes = Vec::new();
        for v in gyro {
            gyro_bytes.extend_from_slice(&v.to_le_bytes());
        }
        memory.write(&mut store, gyro_at, &gyro_bytes).unwrap();
        memory
            .write(&mut store, temp_at, &f64::from(temp).to_le_bytes())
            .unwrap();
        eval.call(&mut store, now).expect("eval");
        let mut panel = vec![0u8; FRAME_BYTES];
        memory.read(&store, ret_at, &mut panel).unwrap();

        assert_eq!(vehicle, panel, "cycle {cycle}: the two hosts diverged");
    }
}
