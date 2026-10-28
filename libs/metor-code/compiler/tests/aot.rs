//! Builds the stand as a shared library, loads it through fsw-3's Dylib
//! loader, and checks it runs exactly as the JIT build does.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use common::{STAND, Sample, Trace, add_rust, config, run};
use metor_code::aot::{LinkOptions, build, library_path};
use metor_code::check::NoImports;
use metor_fsw_3::coordinator::SystemTable;
use metor_fsw_3::metor_proto_wkt::SequenceChannelState;
use metor_fsw_3::{ABI_VERSION, DlPack, Pack, SystemKind};

/// Builds the stand library once per test binary.
fn library() -> &'static Path {
    static BUILT: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (_, path) = BUILT.get_or_init(|| {
        let runtime = metor_code::runtime::runtime_lib(None).expect("the runtime builds");
        let dir = tempfile::tempdir().expect("a temp dir");
        let out = library_path(dir.path(), "stand", None);
        let options = LinkOptions {
            triple: None,
            runtime: &runtime,
            linker: "cc",
        };
        if let Err(error) = build(STAND, &NoImports, &options, &out) {
            panic!("{error}");
        }
        (dir, out)
    });
    path
}

fn open() -> Arc<Pack> {
    // SAFETY: the library was just linked from compiler output and this
    // workspace's mc-rt, against this ABI.
    let pack = unsafe { DlPack::open(library()) }.expect("the library opens");
    Arc::new(Pack::Dl(pack))
}

fn aot_trace(fill: f64, leak: f64, params: serde_json::Value) -> Vec<Sample> {
    let mut table = SystemTable::new();
    table
        .register_pack("stand", open())
        .expect("the pack registers");
    let trace = Trace::default();
    add_rust(&mut table, (fill, leak), &trace);
    let mut coordinator = config("stand.", params)
        .build(&table)
        .expect("the stand builds");
    run(&mut coordinator, 800);
    trace.samples()
}

fn jit_trace(fill: f64, leak: f64, params: serde_json::Value) -> Vec<Sample> {
    let program = metor_code::jit::compile(STAND, &NoImports).expect("compiles");
    let mut table = mc_rt::table(&program).expect("registers");
    let trace = Trace::default();
    add_rust(&mut table, (fill, leak), &trace);
    let mut coordinator = config("", params).build(&table).expect("the stand builds");
    run(&mut coordinator, 800);
    trace.samples()
}

#[test]
fn test_library_exports_the_pack_abi() {
    let pack = open();
    let Pack::Dl(dl) = &*pack else {
        panic!("a library")
    };
    let lib = dl.library();
    // SAFETY: the symbol is the ABI version export, `extern "C" fn() -> u32`.
    let version = unsafe { lib.get::<unsafe extern "C" fn() -> u32>(b"metor_fsw_abi_version") }
        .map(|f| unsafe { f() })
        .expect("the version symbol");
    assert_eq!(version, ABI_VERSION);
    let systems = &pack.descriptor().systems;
    let kinds: Vec<(&str, SystemKind)> = systems.iter().map(|s| (s.ty.as_str(), s.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("leak_check", SystemKind::Sequence),
            ("heater", SystemKind::Cyclic)
        ]
    );
    let leak = &systems[0];
    assert!(leak.doc.starts_with("Pressurises"));
    assert!(
        leak.params
            .as_ref()
            .is_some_and(|p| p.get().contains("\"target\""))
    );
    let inputs: Vec<&str> = leak.def.inputs.iter().map(|p| p.name.as_ref()).collect();
    assert_eq!(inputs, ["abort", "chamber"]);
}

#[test]
fn test_aot_matches_jit_nominal() {
    let aot = aot_trace(5.0, 0.0, serde_json::Value::Null);
    assert_eq!(aot, jit_trace(5.0, 0.0, serde_json::Value::Null));
    let last = aot.iter().rev().find_map(|s| s.state);
    assert_eq!(last, Some(SequenceChannelState::Completed.code()));
}

#[test]
fn test_aot_matches_jit_on_redline_and_leak() {
    let aot = aot_trace(40.0, 0.0, serde_json::Value::Null);
    assert_eq!(aot, jit_trace(40.0, 0.0, serde_json::Value::Null));
    let last = aot.iter().rev().find_map(|s| s.state);
    assert_eq!(last, Some(SequenceChannelState::Aborted.code()));
    let params = serde_json::json!({ "hold": 2.0 });
    let aot = aot_trace(5.0, 0.05, params.clone());
    assert_eq!(aot, jit_trace(5.0, 0.05, params));
    let last = aot.iter().rev().find_map(|s| s.state);
    assert_eq!(last, Some(SequenceChannelState::Failed.code()));
}
