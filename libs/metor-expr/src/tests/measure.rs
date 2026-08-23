//! M4: the numbers the plan asks for, as assertions where there is a budget.
//!
//! Run with `--nocapture` to print them; the plan's Results section is filled
//! in from that output. What is asserted here is only what the plan set a bar
//! for, so this stays a test rather than a benchmark that fails on a busy
//! machine.

use std::time::Instant;

use wasmi::Val;

use super::{build, imports, instantiate};
use crate::{PRELUDE, compile, template::Template};

const ONE_LINER: &str = "def f(x: f64, y: f64) -> f64:\n    return (x * 9.81 + y) / 2.0\n";

fn hundred_lines() -> String {
    let mut source = String::new();
    for i in 0..25 {
        source.push_str(&format!(
            "def step{i}(x: f64, k: f64) -> f64:\n\
             \x20   acc = x * k + {i}.0\n\
             \x20   if acc > 100.0:\n\
             \x20       acc = acc - 100.0\n\
             \x20   return acc\n"
        ));
    }
    source
}

fn median_micros(source: &str, runs: u32) -> f64 {
    let mut samples = Vec::with_capacity(runs as usize);
    for _ in 0..runs {
        let start = Instant::now();
        let module = compile(source).unwrap();
        samples.push(start.elapsed().as_secs_f64() * 1e6);
        std::hint::black_box(module.wasm.len());
    }
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

/// Timings are only meaningful from a release build, so a debug run takes
/// enough samples to report a number and no more.
fn scaled(runs: u32) -> u32 {
    if cfg!(debug_assertions) { runs / 100 } else { runs }
}

/// Cost per evaluation, and fuel per evaluation, driving a module the way a
/// host would.
fn per_eval(wasm: &[u8], name: &str, args: &[Val], runs: u32) -> (f64, u64) {
    let runs = scaled(runs).max(1);
    let (mut store, instance) = instantiate(wasm, u64::MAX / 2);
    let func = instance.get_func(&store, name).unwrap();
    let mut out = [Val::F64(0.0.into())];

    for _ in 0..scaled(1000).max(1) {
        func.call(&mut store, args, &mut out).unwrap();
    }

    let before = store.get_fuel().unwrap();
    func.call(&mut store, args, &mut out).unwrap();
    let fuel = before - store.get_fuel().unwrap();

    let start = Instant::now();
    for _ in 0..runs {
        func.call(&mut store, args, &mut out).unwrap();
    }
    let nanos = start.elapsed().as_secs_f64() * 1e9 / f64::from(runs);
    (nanos, fuel)
}

#[test]
fn compile_latency_is_inside_the_panel_budget() {
    let one = median_micros(ONE_LINER, scaled(200).max(11));
    let many = median_micros(&hundred_lines(), scaled(50).max(5));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!("compile latency ({profile}): one-liner {one:.0} us, 100-line {many:.0} us");

    // The panel debounces at 200 ms; a keystroke must never feel this.
    let budget = if cfg!(debug_assertions) { 20_000.0 } else { 1_000.0 };
    assert!(
        one < budget,
        "a one-liner took {one:.0} us against a {budget:.0} us {profile} budget"
    );
}

#[test]
fn module_size_is_reported_three_ways() {
    let full = PRELUDE.len();
    let scalar = build(ONE_LINER);
    let with_kernels = build("def f(x: f64) -> f64:\n    return sin(x) + exp(x)\n");
    let tensor = build(
        "def f(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> Tensor[f64, 3]:\n    return a * b + a\n",
    );
    let everything = {
        let plan = Template::parse(PRELUDE)
            .unwrap()
            .plan(&[
                "sin", "cos", "tan", "asin", "acos", "atan", "atan2", "exp", "log", "pow",
                "fmod_floor", "k_add", "k_sub", "k_mul", "k_div", "k_neg", "k_dot", "k_matmul",
                "k_sum",
            ])
            .unwrap();
        plan.splice().finish().len()
    };

    println!(
        "module size: prelude {full} B, no kernels reached {} B, transcendentals {} B, \
         tensor kernels {} B, every kernel {everything} B",
        scalar.len(),
        with_kernels.len(),
        tensor.len()
    );

    assert_eq!(imports(&scalar), 0);
    assert!(
        scalar.len() < 10_000,
        "a scalar-only module must stay single-digit KB, got {}",
        scalar.len()
    );
}

#[test]
fn evaluation_cost_is_reported() {
    let scalar = build(ONE_LINER);
    let (nanos, fuel) = per_eval(
        &scalar,
        "f",
        &[Val::F64(3.0.into()), Val::F64(4.0.into())],
        200_000,
    );
    println!("scalar one-liner: {nanos:.0} ns/eval, {fuel} fuel/eval");

    let transcendental = build("def f(x: f64) -> f64:\n    return sin(x) * cos(x)\n");
    let (nanos, fuel) = per_eval(&transcendental, "f", &[Val::F64(0.5.into())], 50_000);
    println!("sin*cos: {nanos:.0} ns/eval, {fuel} fuel/eval");
}

/// The plan's open question: does a length-3 vector op pay a visible tax for
/// going through a kernel instead of being open-coded? Both forms are
/// expressible in the language, so both are compiled and measured, and the
/// answer decides whether small constant shapes should open-code.
#[test]
fn kernel_call_overhead_against_open_coding() {
    // Elementwise, summed to a scalar so both forms return the same thing
    // without either needing a tensor destination.
    let kernel = build(
        "def f(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> f64:\n    return sum(a + b)\n",
    );
    let open = build(
        "def f(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> f64:\n\
         \x20   return (a[0] + b[0]) + (a[1] + b[1]) + (a[2] + b[2])\n",
    );
    let (kernel_ns, kernel_fuel) = per_eval(&kernel, "f", &[], 200_000);
    let (open_ns, open_fuel) = per_eval(&open, "f", &[], 200_000);
    println!(
        "length-3 add+sum: kernel {kernel_ns:.0} ns / {kernel_fuel} fuel, \
         open-coded {open_ns:.0} ns / {open_fuel} fuel"
    );

    for n in [3usize, 8, 32, 128] {
        let kernel = build(&format!(
            "def f(a: Tensor[f64, {n}], b: Tensor[f64, {n}]) -> f64:\n    return dot(a, b)\n"
        ));
        let terms: Vec<String> = (0..n).map(|i| format!("a[{i}] * b[{i}]")).collect();
        let open = build(&format!(
            "def f(a: Tensor[f64, {n}], b: Tensor[f64, {n}]) -> f64:\n    return {}\n",
            terms.join(" + ")
        ));
        let (kernel_ns, kernel_fuel) = per_eval(&kernel, "f", &[], 200_000);
        let (open_ns, open_fuel) = per_eval(&open, "f", &[], 200_000);
        println!(
            "length-{n} dot: kernel {kernel_ns:.0} ns / {kernel_fuel} fuel, \
             open-coded {open_ns:.0} ns / {open_fuel} fuel"
        );
    }
}
