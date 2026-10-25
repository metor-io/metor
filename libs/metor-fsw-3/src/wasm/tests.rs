use std::borrow::Cow;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer, View, Writer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use zerocopy::IntoBytes;

use super::guest::{Guest, Module, Out, TABLE_ELEMENTS};
use super::*;
use crate::coordinator::{EndReason, Step};
use crate::def::DefCx;
use crate::dl::PackError;
use crate::dl::codec::DESCRIPTOR_CAPACITY;
use crate::mirror::mirror_config;
use crate::mirror::tests::{region_bytes, slot_cursor};
use crate::pack::def::Instance;
use crate::pack::raw::{Wasm32RawRing, Wasm32RawSlice};
use crate::system::SystemDef;
use crate::{ABI_VERSION, Input, Output};

/// Returns a module that exports every ABI function.
///
/// `memory` is the imported memory's limits, `extra` is spliced in next to the
/// exports, and `execute` is the body of `metor_fsw_execute`.
fn wat(memory: &str, extra: &str, execute: &str) -> String {
    format!(
        r#"(module
            (import "env" "memory" (memory {memory}))
            {extra}
            (func (export "metor_fsw_abi_version") (result i32) i32.const 15)
            (func (export "metor_fsw_pack_def") (param i32 i32 i32) (result i32) i32.const 0)
            (func (export "metor_fsw_def") (param i32 i32 i32 i32 i32) (result i32) i32.const 0)
            (func (export "metor_fsw_create")
                (param i32 i32 i32 i32 i32 i32 i64 i32 i32 i32 i32) (result i32)
                i32.const 3)
            (func (export "metor_fsw_execute") (param i32 i64) (result i32) {execute})
            (func (export "metor_fsw_alloc") (param i32 i32) (result i32) i32.const 0)
            (func (export "metor_fsw_free") (param i32 i32 i32)))"#
    )
}

fn compile(text: &str) -> Module {
    Module::compile(&wat::parse_str(text).expect("valid wat")).expect("compiles")
}

fn guest(memory: &str, extra: &str, execute: &str, ceiling: usize) -> Guest {
    Guest::new(&compile(&wat(memory, extra, execute)), ceiling, 10_000).expect("instantiates")
}

#[test]
fn test_endless_loop_is_out_of_fuel() {
    let mut guest = guest("1", "", "(loop (br 0)) i32.const 0", PAGE);
    assert_eq!(guest.execute(0, 0, 10_000), Err(Trap::OutOfFuel));
}

#[test]
fn test_store_past_memory_traps_and_spares_the_arena() {
    let store = "i32.const 65536 i32.const 7 i32.store i32.const 0";
    let mut guest = guest("1", "", store, 2 * PAGE);
    let base = guest.region(0, 16).expect("in bounds");
    // SAFETY: the arena is two pages and the memory one, so the canary is in
    // the arena and outside anything the guest can address.
    let canary = unsafe { base.as_ptr().add(PAGE) };
    // SAFETY: as above, and no guest call is running.
    unsafe { canary.write(0xa5) };
    assert_eq!(guest.execute(0, 0, 10_000), Err(Trap::MemoryOutOfBounds));
    // SAFETY: as above.
    assert_eq!(unsafe { canary.read() }, 0xa5);
}

#[test]
fn test_minimum_over_ceiling_is_refused() {
    let module = compile(&wat("17", "", "i32.const 0"));
    assert!(matches!(
        Guest::new(&module, 16 * PAGE, 10_000),
        Err(WasmError::MemoryTooSmall { .. })
    ));
}

#[test]
fn test_grow_stops_at_ceiling_without_moving() {
    let mut guest = guest("1", "", "local.get 0 memory.grow", 4 * PAGE);
    let before = guest.region(0, 16).expect("in bounds");
    assert_eq!(guest.execute(2, 0, 10_000), Ok(1));
    assert_eq!(guest.region(0, 16).expect("in bounds"), before);
    assert!(guest.region(2 * PAGE as u32, 16).is_ok());
    assert_eq!(guest.execute(2, 0, 10_000), Ok(u32::MAX));
    assert!(guest.region(3 * PAGE as u32, 16).is_err());
}

#[test]
fn test_table_grow_stops_at_limit() {
    let grow = "ref.null func local.get 0 table.grow 0";
    let mut guest = guest("1", "(table 1 funcref)", grow, PAGE);
    let past = TABLE_ELEMENTS as u32;
    assert_eq!(guest.execute(past, 0, 10_000), Ok(u32::MAX));
    assert_eq!(guest.execute(1, 0, 10_000), Ok(1));
}

#[test]
fn test_missing_export_is_named() {
    let text = wat("1", "", "i32.const 0").replace(
        r#"(func (export "metor_fsw_alloc") (param i32 i32) (result i32) i32.const 0)"#,
        "",
    );
    assert!(matches!(
        Guest::new(&compile(&text), PAGE, 10_000),
        Err(WasmError::MissingExport("metor_fsw_alloc"))
    ));
}

#[test]
fn test_wrong_signature_is_named() {
    let text = wat("1", "", "i32.const 0").replace(
        r#"(func (export "metor_fsw_free") (param i32 i32 i32))"#,
        r#"(func (export "metor_fsw_free") (param i32))"#,
    );
    assert!(matches!(
        Guest::new(&compile(&text), PAGE, 10_000),
        Err(WasmError::Signature("metor_fsw_free"))
    ));
}

#[test]
fn test_memory_import_required() {
    let text =
        wat("1", "", "i32.const 0").replace(r#"(import "env" "memory" (memory 1))"#, "(memory 1)");
    assert!(matches!(
        Guest::new(&compile(&text), PAGE, 10_000),
        Err(WasmError::MemoryImport)
    ));
}

#[test]
fn test_bad_ceiling() {
    let module = compile(&wat("0", "", "i32.const 0"));
    assert!(matches!(
        Guest::new(&module, 0, 10_000),
        Err(WasmError::BadCeiling(0))
    ));
}

#[test]
fn test_accesses_stay_in_memory() {
    let mut guest = guest("1", "", "i32.const 0", 2 * PAGE);
    let end = PAGE as u32;
    guest.write(end - 4, b"meto").expect("in bounds");
    let mut read = [0; 4];
    guest.read(end - 4, &mut read).expect("in bounds");
    assert_eq!(&read, b"meto");

    assert!(matches!(
        guest.write(end - 3, b"meto"),
        Err(WasmError::OutOfBounds { .. })
    ));
    assert!(matches!(
        guest.read(u32::MAX, &mut read),
        Err(WasmError::OutOfBounds { .. })
    ));
    assert!(matches!(
        guest.region(end, 16),
        Err(WasmError::OutOfBounds { .. })
    ));
    assert!(matches!(guest.region(8, 16), Err(WasmError::Misaligned(8))));
    assert!(guest.region(end - 16, 16).is_ok());
}

#[test]
fn test_calls_reach_each_export() {
    let mut guest = guest("1", "", "i32.const 4", PAGE);
    assert_eq!(guest.abi_version(1_000), Ok(15));
    assert_eq!(guest.execute(0, 0, 1_000), Ok(4));
    assert_eq!(guest.alloc(16, 16, 1_000), Ok(0));
    assert_eq!(guest.free(0, 16, 16, 1_000), Ok(()));
    let out = Out {
        dst: 0,
        capacity: 0,
        written: 0,
    };
    assert_eq!(guest.pack_def(out, 1_000), Ok(0));
    assert_eq!(guest.def(0, 0, out, 1_000), Ok(0));
    let create = (0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0);
    assert_eq!(guest.create(create, 1_000), Ok(3));
}

fn open(text: &str) -> Result<WasmPack, PackError> {
    WasmPack::open(
        &wat::parse_str(text).expect("valid wat"),
        &Limits::default(),
    )
}

#[test]
fn test_wrong_abi_is_mismatch() {
    let text = wat("1", "", "i32.const 0")
        .replace("(result i32) i32.const 15)", "(result i32) i32.const 1)");
    assert!(matches!(
        open(&text),
        Err(PackError::AbiMismatch {
            found: 1,
            expected: ABI_VERSION
        })
    ));
}

#[test]
fn test_refused_allocation_fails_open() {
    let capacity = DESCRIPTOR_CAPACITY as u32;
    assert!(matches!(
        open(&wat("1", "", "i32.const 0")),
        Err(PackError::Wasm(WasmError::Alloc(len))) if len == capacity
    ));
}

#[test]
fn test_raw_slice_layout() {
    let slice = Wasm32RawSlice::new(0x0403_0201, 5);
    assert_eq!(slice.as_bytes(), [1, 2, 3, 4, 5, 0, 0, 0]);
}

#[test]
fn test_raw_ring_layout() {
    let ring = Wasm32RawRing::new(0x0403_0201, 0x40);
    let mut golden = [0; 20];
    golden[..5].copy_from_slice(&[1, 2, 3, 4, 0x40]);
    assert_eq!(ring.as_bytes(), golden);
}

/// Returns a guest module for step tests.
///
/// The guest describes an empty pack and uses a bump allocator. At `create`, it
/// saves the bases of its first input and output rings in `$in` and `$out`.
/// `execute` is its step body.
fn step_wat(execute: &str) -> String {
    format!(
        r#"(module
            (import "env" "memory" (memory 32))
            (global $heap (mut i32) (i32.const 1024))
            (global $in (mut i32) (i32.const 0))
            (global $out (mut i32) (i32.const 0))
            (data (i32.const 16) "{json}")
            (func (export "metor_fsw_abi_version") (result i32) i32.const 15)
            (func (export "metor_fsw_pack_def") (param $dst i32) (param i32) (param $written i32)
                (result i32)
                (memory.copy (local.get $dst) (i32.const 16) (i32.const {len}))
                (i32.store (local.get $written) (i32.const {len}))
                i32.const 0)
            (func (export "metor_fsw_def") (param i32 i32 i32 i32 i32) (result i32) i32.const 2)
            (func (export "metor_fsw_create")
                (param i32 i32 i32 i32 i32 i32 i64 i32 i32 i32 i32) (result i32)
                (if (i32.load offset=4 (local.get 3))
                    (then (global.set $in (i32.load (i32.load (i32.load (local.get 3)))))))
                (if (i32.load offset=4 (local.get 4))
                    (then (global.set $out (i32.load (i32.load (local.get 4))))))
                (i32.store (local.get 7) (i32.const 1))
                i32.const 0)
            (func (export "metor_fsw_execute") (param i32 i64) (result i32) {execute})
            (func (export "metor_fsw_alloc") (param $len i32) (param $align i32) (result i32)
                (local $at i32)
                (local.set $at
                    (i32.and
                        (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
                        (i32.sub (i32.const 0) (local.get $align))))
                (global.set $heap (i32.add (local.get $at) (local.get $len)))
                local.get $at)
            (func (export "metor_fsw_free") (param i32 i32 i32)))"#,
        json = r#"{\"systems\":[]}"#,
        len = r#"{"systems":[]}"#.len(),
    )
}

const RING: usize = 256;
const LOG_RING: usize = 1 << 16;

/// Returns wat stores that write `record` into a fresh copy of a `RING`-byte
/// output at `$out`.
///
/// If `corrupt` is set, the record's length is written as `u32::MAX`.
fn record_stores(record: &[u8], corrupt: bool) -> String {
    let config = mirror_config(RING, 2, 1).expect("fits");
    let ring = RingBuffer::create_in_memory(config).expect("a valid ring config");
    let _host = ring.view(NoWake).expect("a free reader slot");
    let before = region_bytes(&ring);
    let mut writer = ring.writer(NoWake).expect("a free writer");
    writer.try_write(record).expect("room");
    drop(writer);
    let mut after = region_bytes(&ring);
    if corrupt {
        let header = after.len() - config.capacity;
        after[header..header + 4].fill(0xff);
    }
    let changed = before.iter().zip(&after).enumerate();
    changed
        .filter(|(_, (before, after))| before != after)
        .map(|(at, (_, byte))| {
            format!("(i32.store8 (i32.add (global.get $out) (i32.const {at})) (i32.const {byte}))")
        })
        .collect()
}

/// A `WasmStep` running a `step_wat` guest, plus the host ends of its rings.
struct Rig {
    step: WasmStep,
    input: RingBuffer,
    feed: Writer<NoWake>,
    output: View<NoWake>,
    log: Input<LogEvent>,
}

fn ring(capacity: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity,
        max_readers: 1,
    })
    .expect("a valid ring config")
}

fn rig(execute: &str) -> Rig {
    let limits = Limits {
        memory: Some(64 * PAGE),
        fuel: Some(100_000),
    };
    let bytes = wat::parse_str(step_wat(execute)).expect("valid wat");
    let pack = WasmPack::open(&bytes, &limits).expect("opens");
    let (input, output, log) = (ring(RING), ring(RING), ring(LOG_RING));
    let cx = DefCx::empty();
    let def = SystemDef {
        name: "guest".into(),
        inputs: vec![Input::<LogEvent>::def(&cx, "input")],
        outputs: vec![
            Output::<LogEvent>::def(&cx, "output"),
            Output::<LogEvent>::def(&cx, "log"),
        ],
        log: Some(1),
        grants: Vec::new(),
        io: None,
    };
    let instance = Instance {
        id: "guest".into(),
        def,
        adapter: None,
        seed: 0,
    };
    let (seen, lines) = (output.view(NoWake), log.view(NoWake));
    let (inputs, outputs) = ([vec![&input]], [&output, &log]);
    let step = pack
        .create(
            "guest",
            &serde_json::Value::Null,
            &instance,
            crate::dl::InstanceRings {
                inputs: &inputs,
                outputs: &outputs,
            },
            &mut Default::default(),
        )
        .expect("creates");
    Rig {
        step,
        feed: input.writer(NoWake).expect("a free writer"),
        input,
        output: seen.expect("a free reader slot"),
        log: Input::new(vec![lines.expect("a free reader slot")]),
    }
}

impl Rig {
    fn execute(&mut self) {
        let mut io = Default::default();
        self.step
            .execute(Timestamp(1), &mut crate::coordinator::StepCx::new(&mut io));
    }

    /// Returns every log line as `kind: message`.
    fn lines(&mut self) -> Vec<String> {
        let lines: Vec<LogEvent> = self.log.drain().flatten().collect();
        lines
            .iter()
            .map(|line| {
                let kind = line.fields.iter().find(|(name, _)| name == "kind");
                let kind = kind.map_or("", |(_, value)| value.as_ref());
                format!("{kind}: {}", line.message)
            })
            .collect()
    }
}

#[test]
fn test_each_end() {
    let stores = record_stores(b"ping", false);
    let corrupt = record_stores(b"ping", true);
    let failed = |why: &'static str| Some(EndReason::Failed(Cow::Borrowed(why)));
    let rows: [(String, Option<EndReason>, &[&str]); 9] = [
        ("i32.const 0".into(), None, &[]),
        ("i32.const 2".into(), Some(EndReason::Completed), &[]),
        ("i32.const 3".into(), Some(EndReason::Aborted), &[]),
        ("i32.const 4".into(), failed("see the system's log"), &[]),
        (
            "i32.const 9".into(),
            Some(EndReason::Panicked),
            &["bad_status: the guest returned no known status"],
        ),
        (
            "unreachable".into(),
            Some(EndReason::Panicked),
            &["trap: unreachable"],
        ),
        (
            "(loop (br 0)) i32.const 0".into(),
            failed("out of fuel"),
            &["fuel: the guest ran out of fuel"],
        ),
        (
            format!("{corrupt} i32.const 0"),
            failed("corrupt ring"),
            &["corrupt: a guest ring is corrupt"],
        ),
        (format!("{stores} i32.const 0"), None, &[]),
    ];
    for (execute, end, lines) in rows {
        let mut rig = rig(&execute);
        rig.execute();
        assert_eq!(rig.step.ended(), end.as_ref(), "{execute}");
        assert_eq!(rig.lines(), lines, "{execute}");
    }
}

#[test]
fn test_corrupt_input_ring_ends_the_step() {
    let cursor = slot_cursor(mirror_config(RING, 2, 1).expect("fits"));
    let at = format!("(i32.add (global.get $in) (i32.const {cursor}))");
    let mut rig = rig(&format!("(i64.store {at} (i64.const -2)) i32.const 0"));
    rig.execute();
    assert_eq!(rig.step.ended(), None);
    rig.feed.try_write(b"fed").expect("room");
    rig.execute();
    let corrupt = EndReason::Failed(Cow::Borrowed("corrupt ring"));
    assert_eq!(rig.step.ended(), Some(&corrupt));
    assert_eq!(rig.lines(), ["corrupt: a guest ring is corrupt"]);
}

#[test]
fn test_outputs_drained_after_trap() {
    let mut rig = rig(&format!("{} unreachable", record_stores(b"ping", false)));
    rig.execute();
    assert_eq!(rig.step.ended(), Some(&EndReason::Panicked));
    let records: Vec<Vec<u8>> = rig
        .output
        .drain()
        .map(|r| r.expect("whole").to_vec())
        .collect();
    assert_eq!(records, [b"ping".to_vec()]);
}

#[test]
fn test_end_frees_input_reader_slot() {
    let mut rig = rig("unreachable");
    rig.feed.try_write(b"fed").expect("room");
    assert!(rig.input.view(NoWake).is_err());
    rig.execute();
    assert!(rig.input.view(NoWake).is_ok());
    rig.execute();
    assert_eq!(rig.lines(), ["trap: unreachable"]);
}
