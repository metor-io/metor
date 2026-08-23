//! The harness every test in this crate runs through.
//!
//! There is one rule and it shapes all of these: **a compiled module is only
//! believed once it has run.** Nothing here asserts on bytes or on IR. Every
//! test compiles source, instantiates the result under fuel-metered `wasmi` —
//! the same interpreter, with the same fuel discipline, the hosts use — and
//! calls the exported function.
//!
//! [`differential`] adds the second half of that rule for the semantic tests:
//! the same computation is also evaluated natively against nox in-process, and
//! the two must agree bit for bit.

mod abi;
mod differential;
mod m0;
mod measure;
mod robustness;
mod scalar;
mod semantics;
mod systems;
mod tensors;

use wasmi::{Engine, Instance, Linker, Module, Store, Val};

use crate::{Diagnostics, compile};

/// Instantiate a compiled module with a fuel budget, as the hosts will.
pub(super) fn instantiate(wasm: &[u8], fuel: u64) -> (Store<()>, Instance) {
    let mut config = wasmi::Config::default();
    config.compilation_mode(wasmi::CompilationMode::Eager);
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm).expect("compiled module must validate");
    let mut store = Store::new(&engine, ());
    store.set_fuel(fuel).unwrap();
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    (store, instance)
}

pub(super) fn call(wasm: &[u8], name: &str, args: &[Val]) -> Result<Val, wasmi::Error> {
    let (mut store, instance) = instantiate(wasm, 10_000_000);
    let func = instance.get_func(&store, name).expect("export is present");
    let mut out = [Val::I32(0)];
    func.call(&mut store, args, &mut out)?;
    Ok(out[0].clone())
}

/// An `f64` argument.
pub(super) fn fv(v: f64) -> Val {
    Val::F64(v.into())
}

/// An `i64` argument.
pub(super) fn iv(v: i64) -> Val {
    Val::I64(v)
}

pub(super) fn run_f64(wasm: &[u8], name: &str, args: &[Val]) -> f64 {
    match call(wasm, name, args).unwrap() {
        Val::F64(v) => v.into(),
        other => panic!("expected an f64 result, got {other:?}"),
    }
}

pub(super) fn run_i64(wasm: &[u8], name: &str, args: &[Val]) -> i64 {
    match call(wasm, name, args).unwrap() {
        Val::I64(v) => v,
        other => panic!("expected an i64 result, got {other:?}"),
    }
}

pub(super) fn run_bool(wasm: &[u8], name: &str, args: &[Val]) -> bool {
    match call(wasm, name, args).unwrap() {
        Val::I32(v) => v != 0,
        other => panic!("expected a bool result, got {other:?}"),
    }
}

pub(super) fn call_f64(wasm: &[u8], name: &str, args: &[f64]) -> f64 {
    let args: Vec<Val> = args.iter().map(|a| Val::F64((*a).into())).collect();
    match call(wasm, name, &args).unwrap() {
        Val::F64(v) => v.into(),
        other => panic!("expected an f64 result, got {other:?}"),
    }
}

pub(super) fn imports(wasm: &[u8]) -> usize {
    wasmparser::Parser::new(0)
        .parse_all(wasm)
        .filter_map(|p| match p.unwrap() {
            wasmparser::Payload::ImportSection(r) => Some(r.count() as usize),
            _ => None,
        })
        .sum()
}

pub(super) fn defined_functions(wasm: &[u8]) -> usize {
    wasmparser::Parser::new(0)
        .parse_all(wasm)
        .filter_map(|p| match p.unwrap() {
            wasmparser::Payload::FunctionSection(r) => Some(r.count() as usize),
            _ => None,
        })
        .sum()
}

/// Compile a source string, insisting it is accepted, and check the module
/// stays closed.
pub(super) fn build(source: &str) -> Vec<u8> {
    let module = match compile(source) {
        Ok(module) => module,
        Err(diags) => panic!("expected {source:?} to compile, got:\n{diags}"),
    };
    assert_eq!(
        imports(&module.wasm),
        0,
        "compiled modules must stay closed"
    );
    module.wasm
}

/// Compile a source string, insisting it is refused.
pub(super) fn reject(source: &str) -> Diagnostics {
    match compile(source) {
        Ok(_) => panic!("expected {source:?} to be refused"),
        Err(diags) => diags,
    }
}

/// Wrap an expression as a single-argument `f64` function and run it.
pub(super) fn eval_f64(expr: &str) -> f64 {
    let source = format!("def f() -> f64:\n    return {expr}\n");
    call_f64(&build(&source), "f", &[])
}

/// Wrap an expression as a single-argument `i64` function and run it.
pub(super) fn eval_i64(expr: &str) -> i64 {
    let source = format!("def f() -> i64:\n    return {expr}\n");
    match call(&build(&source), "f", &[]).unwrap() {
        Val::I64(v) => v,
        other => panic!("expected an i64 result, got {other:?}"),
    }
}

/// Wrap an expression as a `bool` function and run it.
pub(super) fn eval_bool(expr: &str) -> bool {
    let source = format!("def f() -> bool:\n    return {expr}\n");
    match call(&build(&source), "f", &[]).unwrap() {
        Val::I32(v) => v != 0,
        other => panic!("expected a bool result, got {other:?}"),
    }
}

/// Run something expected to trap, returning the trap's text.
pub(super) fn trap(wasm: &[u8], name: &str, args: &[Val]) -> String {
    match call(wasm, name, args) {
        Ok(v) => panic!("expected a trap, got {v:?}"),
        Err(e) => format!("{e}"),
    }
}
