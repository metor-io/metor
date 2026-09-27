use super::abi::raw_slice;
use super::*;
use crate::ABI_VERSION;
use crate::dl::PackError;
use crate::dl::codec::DESCRIPTOR_CAPACITY;

/// A module exporting every ABI function over `memory`, with `extra` beside
/// them and `execute` as `metor_fsw_execute`'s body.
fn wat(memory: &str, extra: &str, execute: &str) -> String {
    format!(
        r#"(module
            (import "env" "memory" (memory {memory}))
            {extra}
            (func (export "metor_fsw_abi_version") (result i32) i32.const 9)
            (func (export "metor_fsw_pack_def") (param i32 i32 i32) (result i32) i32.const 0)
            (func (export "metor_fsw_def") (param i32 i32 i32 i32 i32) (result i32) i32.const 0)
            (func (export "metor_fsw_create") (param i32 i32 i32 i32 i32 i32) (result i32)
                i32.const 0)
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
    // SAFETY: as above; no guest call is running.
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
    assert_eq!(guest.abi_version(1_000), Ok(9));
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
}

const FUEL: Fuel = Fuel {
    step: 10_000,
    setup: 10_000,
};

fn open(text: &str) -> Result<WasmPack, PackError> {
    WasmPack::open(&wat::parse_str(text).expect("valid wat"), PAGE, FUEL)
}

#[test]
fn test_wrong_abi_is_mismatch() {
    let text = wat("1", "", "i32.const 0")
        .replace("(result i32) i32.const 9)", "(result i32) i32.const 1)");
    assert!(matches!(
        open(&text),
        Err(PackError::AbiMismatch {
            found: 1,
            expected: ABI_VERSION
        })
    ));
}

#[test]
fn test_missing_export_is_missing_symbol() {
    let text = wat("1", "", "i32.const 0").replace(
        r#"(func (export "metor_fsw_pack_def") (param i32 i32 i32) (result i32) i32.const 0)"#,
        "",
    );
    assert!(matches!(
        open(&text),
        Err(PackError::MissingSymbol("metor_fsw_pack_def"))
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
    assert_eq!(raw_slice(0x0403_0201, 5), [1, 2, 3, 4, 5, 0, 0, 0]);
}
