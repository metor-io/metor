//! M0: the prelude spike.
//!
//! These are the checks that decided the template approach — that a generated
//! function calling a prelude kernel runs under the same fuel-metered wasmi the
//! hosts use, that dropping unreachable kernels leaves a valid module, and that
//! the result stays closed.

use wasm_encoder::{Function, Instruction, ValType};

use super::{call_f64, defined_functions, imports, instantiate};
use crate::{PRELUDE, template::Template};

#[test]
fn prelude_is_closed_and_exports_its_kernels() {
    assert_eq!(imports(PRELUDE), 0);
    let template = Template::parse(PRELUDE).unwrap();
    for kernel in [
        "sin",
        "cos",
        "tan",
        "asin",
        "acos",
        "atan",
        "atan2",
        "exp",
        "log",
        "pow",
        "fmod_floor",
        "k_add",
        "k_sub",
        "k_mul",
        "k_div",
        "k_neg",
        "k_dot",
        "k_matmul",
        "k_sum",
    ] {
        assert!(template.export(kernel).is_some(), "missing kernel {kernel}");
    }
}

/// The template proof: a hand-built function calling a prelude kernel, run
/// under wasmi.
#[test]
fn generated_function_calls_a_prelude_kernel() {
    let plan = Template::parse(PRELUDE)
        .unwrap()
        .plan(&["sin", "pow"])
        .unwrap();
    let mut splice = plan.splice();

    let sin = splice.kernel("sin");
    let pow = splice.kernel("pow");
    let ty = splice.ty(vec![ValType::F64, ValType::F64], vec![ValType::F64]);

    // sin(x) + x ** y
    let mut body = Function::new([]);
    body.instruction(&Instruction::LocalGet(0));
    body.instruction(&Instruction::Call(sin));
    body.instruction(&Instruction::LocalGet(0));
    body.instruction(&Instruction::LocalGet(1));
    body.instruction(&Instruction::Call(pow));
    body.instruction(&Instruction::F64Add);
    body.instruction(&Instruction::End);

    let index = splice.function(ty, body);
    splice.export("expr_eval", index);
    let wasm = splice.finish();

    assert_eq!(imports(&wasm), 0, "compiled modules must stay closed");
    let got = call_f64(&wasm, "expr_eval", &[0.5, 3.0]);
    let want = libm::sin(0.5) + libm::pow(0.5, 3.0);
    assert_eq!(got, want);
}

/// A generated function reading and writing the arena through a spliced data
/// segment — the path tensor arguments will take.
#[test]
fn generated_function_drives_a_tensor_kernel() {
    let plan = Template::parse(PRELUDE)
        .unwrap()
        .plan(&["k_add", "k_dot"])
        .unwrap();
    let mut splice = plan.splice();

    let base = arena_base();
    let a = base;
    let b = base + 64;
    let out = base + 128;
    let desc = base + 192;

    splice.data(a, [1.0f64, 2.0, 3.0].map(f64::to_le_bytes).concat());
    splice.data(b, [10.0f64, 20.0, 30.0].map(f64::to_le_bytes).concat());

    let mut d = Vec::new();
    for word in [a, b, out, 1u32] {
        d.extend_from_slice(&word.to_le_bytes());
    }
    for shape in [[3u32, 0, 0, 0], [3, 0, 0, 0], [3, 0, 0, 0]] {
        for axis in shape {
            d.extend_from_slice(&axis.to_le_bytes());
        }
    }
    splice.data(desc, d);

    let k_add = splice.kernel("k_add");
    let k_dot = splice.kernel("k_dot");
    let ty = splice.ty(vec![], vec![ValType::F64]);

    // dot(a + b, a)
    let mut body = Function::new([]);
    body.instruction(&Instruction::I32Const(desc as i32));
    body.instruction(&Instruction::Call(k_add));
    body.instruction(&Instruction::I32Const(out as i32));
    body.instruction(&Instruction::I32Const(a as i32));
    body.instruction(&Instruction::I32Const(3));
    body.instruction(&Instruction::Call(k_dot));
    body.instruction(&Instruction::End);

    let index = splice.function(ty, body);
    splice.export("expr_eval", index);
    let wasm = splice.finish();

    let got = call_f64(&wasm, "expr_eval", &[]);
    assert_eq!(got, 11.0 * 1.0 + 22.0 * 2.0 + 33.0 * 3.0);
}

/// The first address the compiler may place a buffer at, straight from the
/// linker.
fn arena_base() -> u32 {
    Template::parse(PRELUDE).unwrap().heap_base()
}

/// The GC proof: asking for one kernel keeps far less than asking for all of
/// them, and both still run.
#[test]
fn unreachable_kernels_are_dropped() {
    let full = defined_functions(PRELUDE);

    let lean = Template::parse(PRELUDE).unwrap().plan(&["sin"]).unwrap();
    let lean_kept = lean.kept() as usize;
    let mut splice = lean.splice();
    let sin = splice.kernel("sin");
    let ty = splice.ty(vec![ValType::F64], vec![ValType::F64]);
    let mut body = Function::new([]);
    body.instruction(&Instruction::LocalGet(0));
    body.instruction(&Instruction::Call(sin));
    body.instruction(&Instruction::End);
    let index = splice.function(ty, body);
    splice.export("expr_eval", index);
    let lean_wasm = splice.finish();

    assert!(
        lean_kept < full,
        "GC kept every function ({lean_kept} of {full})"
    );
    assert_eq!(call_f64(&lean_wasm, "expr_eval", &[0.25]), libm::sin(0.25));

    // Every kernel a program can reach is still callable after the walk.
    let all: Vec<&str> = vec![
        "sin",
        "cos",
        "tan",
        "asin",
        "acos",
        "atan",
        "atan2",
        "exp",
        "log",
        "pow",
        "fmod_floor",
        "k_add",
        "k_sub",
        "k_mul",
        "k_div",
        "k_neg",
        "k_dot",
        "k_matmul",
        "k_sum",
    ];
    let plan = Template::parse(PRELUDE).unwrap().plan(&all).unwrap();
    let fat_kept = plan.kept() as usize;
    assert!(lean_kept < fat_kept);
    let mut splice = plan.splice();
    let exp = splice.kernel("exp");
    let ty = splice.ty(vec![ValType::F64], vec![ValType::F64]);
    let mut body = Function::new([]);
    body.instruction(&Instruction::LocalGet(0));
    body.instruction(&Instruction::Call(exp));
    body.instruction(&Instruction::End);
    let index = splice.function(ty, body);
    splice.export("expr_eval", index);
    let fat_wasm = splice.finish();
    assert_eq!(call_f64(&fat_wasm, "expr_eval", &[1.0]), libm::exp(1.0));
    assert!(lean_wasm.len() < fat_wasm.len());
}

/// Fuel bounds a compiled module exactly as it bounds a pack occupant.
#[test]
fn a_runaway_generated_function_burns_its_grant() {
    let plan = Template::parse(PRELUDE).unwrap().plan(&[]).unwrap();
    let mut splice = plan.splice();
    let ty = splice.ty(vec![], vec![ValType::F64]);
    let mut body = Function::new([]);
    body.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    body.instruction(&Instruction::Br(0));
    body.instruction(&Instruction::End);
    body.instruction(&Instruction::F64Const(0.0.into()));
    body.instruction(&Instruction::End);
    let index = splice.function(ty, body);
    splice.export("expr_eval", index);
    let wasm = splice.finish();

    let (mut store, instance) = instantiate(&wasm, 10_000);
    let func = instance.get_func(&store, "expr_eval").unwrap();
    let mut out = [wasmi::Val::F64(0.0.into())];
    let err = func.call(&mut store, &[], &mut out).unwrap_err();
    assert!(
        format!("{err}").contains("fuel"),
        "expected a fuel exhaustion trap, got {err}"
    );
}

/// Elementwise kernels agree with native nox at the shapes nox is correct at.
#[test]
fn kernels_agree_with_native_nox() {
    use nox::{ArrayRepr, Const, ReprMonad, Tensor};

    let a = [1.5f64, -2.25, 3.75];
    let b = [0.5f64, 4.0, -1.25];

    let plan = Template::parse(PRELUDE)
        .unwrap()
        .plan(&["k_mul", "k_dot"])
        .unwrap();
    let base = arena_base();
    let mut splice = plan.splice();
    let (pa, pb, pout, pdesc) = (base, base + 64, base + 128, base + 192);
    splice.data(pa, a.map(f64::to_le_bytes).concat().to_vec());
    splice.data(pb, b.map(f64::to_le_bytes).concat().to_vec());
    let mut d = Vec::new();
    for word in [pa, pb, pout, 1u32] {
        d.extend_from_slice(&word.to_le_bytes());
    }
    for _ in 0..3 {
        for axis in [3u32, 0, 0, 0] {
            d.extend_from_slice(&axis.to_le_bytes());
        }
    }
    splice.data(pdesc, d);

    let k_mul = splice.kernel("k_mul");
    let k_dot = splice.kernel("k_dot");
    let ty = splice.ty(vec![], vec![ValType::F64]);
    let mut body = Function::new([]);
    body.instruction(&Instruction::I32Const(pdesc as i32));
    body.instruction(&Instruction::Call(k_mul));
    body.instruction(&Instruction::I32Const(pout as i32));
    body.instruction(&Instruction::I32Const(pb as i32));
    body.instruction(&Instruction::I32Const(3));
    body.instruction(&Instruction::Call(k_dot));
    body.instruction(&Instruction::End);
    let index = splice.function(ty, body);
    splice.export("expr_eval", index);
    let wasm = splice.finish();

    let ta: Tensor<f64, Const<3>, ArrayRepr> = a.into();
    let tb: Tensor<f64, Const<3>, ArrayRepr> = b.into();
    let want = (ta * tb).dot(&tb).into_inner().view().buf()[0];

    assert_eq!(call_f64(&wasm, "expr_eval", &[]), want);
}

/// The plan's acceptance numbers, as an assertion rather than a note: a
/// scalar-only module stays single-digit KB, every module stays closed, and
/// asking for more kernels costs monotonically more bytes.
#[test]
fn module_size_tracks_what_the_program_reaches() {
    let sizes: Vec<(usize, usize)> = [
        vec![],
        vec!["sin"],
        vec![
            "sin", "cos", "tan", "asin", "acos", "atan", "atan2", "exp", "log", "pow",
        ],
        vec![
            "sin",
            "cos",
            "tan",
            "asin",
            "acos",
            "atan",
            "atan2",
            "exp",
            "log",
            "pow",
            "fmod_floor",
            "k_add",
            "k_sub",
            "k_mul",
            "k_div",
            "k_neg",
            "k_dot",
            "k_matmul",
            "k_sum",
        ],
    ]
    .into_iter()
    .map(|kernels| {
        let plan = Template::parse(PRELUDE).unwrap().plan(&kernels).unwrap();
        let kept = plan.kept() as usize;
        let mut splice = plan.splice();
        let ty = splice.ty(vec![ValType::F64, ValType::F64], vec![ValType::F64]);
        let mut body = Function::new([]);
        body.instruction(&Instruction::LocalGet(0));
        body.instruction(&Instruction::LocalGet(1));
        body.instruction(&Instruction::F64Mul);
        body.instruction(&Instruction::End);
        let index = splice.function(ty, body);
        splice.export("expr_eval", index);
        let wasm = splice.finish();
        assert_eq!(imports(&wasm), 0);
        assert_eq!(call_f64(&wasm, "expr_eval", &[3.0, 4.0]), 12.0);
        (kept, wasm.len())
    })
    .collect();

    let (scalar_kept, scalar_bytes) = sizes[0];
    assert!(
        scalar_bytes < 10_000,
        "a scalar-only module must stay single-digit KB, got {scalar_bytes}"
    );
    assert!(scalar_kept < defined_functions(PRELUDE));
    for pair in sizes.windows(2) {
        assert!(
            pair[0].1 < pair[1].1,
            "reaching more kernels must not shrink the module: {pair:?}"
        );
    }
}
