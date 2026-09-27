//! The host side against `echo-pack` built as a module.

mod common;

use common::{fixture_lib, fixture_wasm};
use metor_fsw_3::wasm::{Fuel, WasmPack};
use metor_fsw_3::{DefCx, DlPack};
use serde_json::{Value, json};

/// Each guest's memory ceiling.
const MEMORY: usize = 16 << 20;

const FUEL: Fuel = Fuel {
    step: 10_000_000,
    setup: 1_000_000_000,
};

fn open_wasm() -> WasmPack {
    WasmPack::open(fixture_wasm(), MEMORY, FUEL).expect("the module opens")
}

fn open_dylib() -> DlPack {
    // SAFETY: the fixture is a metor-fsw-3 pack this workspace just built.
    unsafe { DlPack::open(fixture_lib()) }.expect("the fixture opens")
}

fn value(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("encodes")
}

#[test]
fn test_descriptor_matches_dylib() {
    let (wasm, dylib) = (open_wasm(), open_dylib());
    assert_eq!(value(wasm.descriptor()), value(dylib.descriptor()));
    let kinds: Vec<_> = wasm.descriptor().systems.iter().map(|s| s.kind).collect();
    assert!(kinds.contains(&metor_fsw_3::SystemKind::Async));
    assert!(kinds.contains(&metor_fsw_3::SystemKind::Sequence));
}

#[test]
fn test_def_matches_dylib() {
    let params = json!({ "gain": 2.0 });
    let cx = DefCx {
        id: "scaled",
        params: &params,
        ..DefCx::empty()
    };
    let wasm = open_wasm().def("gain", &cx).expect("a definition");
    assert_eq!(wasm, open_dylib().def("gain", &cx).expect("a definition"));
}

#[test]
fn test_def_of_unknown_type_is_refused() {
    let refused = open_wasm().def("nope", &DefCx::empty());
    assert!(matches!(refused, Err(metor_fsw_3::DefError::Pack { .. })));
}

/// rustc passes a by-value `RawSlice` as an `i32` pointer; a change there
/// fails here first.
#[test]
fn test_export_signatures() {
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, fixture_wasm()).expect("compiles");
    let mut exports: Vec<String> = module
        .exports()
        .filter(|export| export.name().starts_with("metor_fsw_"))
        .map(|export| {
            let ty = export.ty().func().expect("a function");
            format!("{} {:?} -> {:?}", export.name(), ty.params(), ty.results())
        })
        .collect();
    exports.sort();
    assert_eq!(
        exports,
        [
            "metor_fsw_abi_version [] -> [I32]",
            "metor_fsw_alloc [I32, I32] -> [I32]",
            "metor_fsw_create [I32, I32, I32, I32, I32, I32] -> [I32]",
            "metor_fsw_def [I32, I32, I32, I32, I32] -> [I32]",
            "metor_fsw_destroy [I32] -> []",
            "metor_fsw_execute [I32, I64] -> [I32]",
            "metor_fsw_free [I32, I32, I32] -> []",
            "metor_fsw_pack_def [I32, I32, I32] -> [I32]",
        ]
    );
    let imports: Vec<_> = module
        .imports()
        .map(|import| format!("{}.{}", import.module(), import.name()))
        .collect();
    assert_eq!(imports, ["env.memory"]);
}
