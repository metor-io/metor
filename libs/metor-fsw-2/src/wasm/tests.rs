//! Drive the real `seq-fixture` pack, compiled to wasm, through the whole ABI
//! lifecycle — and prove the two properties the substrate exists for: a poll
//! is bounded by fuel, and a fault is contained.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use metor_fsw_2_core::abi::{ROLE_INPUT, ROLE_OUTPUT};
use metor_fsw_2_core::{SequenceStatus, SlotControlIn, capacity_for};
use metor_fsw_ring::Config;

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
/// opaque trap because the module imports nothing and the target aborts rather
/// than unwinds, so `catch_unwind` cannot turn the panic into a status word.
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
    assert_eq!(status, FswStatus::Done, "the waiter reaches a terminal state");

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



