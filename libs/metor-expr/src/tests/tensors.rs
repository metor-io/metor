//! M3: tensors, through static buffers, against native nox.
//!
//! A tensor-using function is driven the way the hosts will drive it — write
//! the arguments at `<name>_arg_ptr(i)`, call `<name>()`, read the result at
//! `<name>_ret_ptr()` — and whole results are compared elementwise against
//! nox at fixed shapes.

use nox::{ArrayRepr, Const, ReprMonad, Tensor};
use wasmi::{Instance, Store, Val};

use super::{build, instantiate, reject};
use crate::{Dtype, Ty, compile};

/// Drive a buffer-ABI function: arguments in, call, result out.
struct Driver {
    store: Store<()>,
    instance: Instance,
    name: String,
}

impl Driver {
    fn new(wasm: &[u8], name: &str) -> Self {
        let (store, instance) = instantiate(wasm, 100_000_000);
        Driver {
            store,
            instance,
            name: name.to_string(),
        }
    }

    fn address(&mut self, accessor: &str, index: Option<i32>) -> u32 {
        let func = self
            .instance
            .get_func(&self.store, accessor)
            .unwrap_or_else(|| panic!("missing {accessor}"));
        let args: Vec<Val> = index.map(Val::I32).into_iter().collect();
        let mut out = [Val::I32(0)];
        func.call(&mut self.store, &args, &mut out).unwrap();
        match &out[0] {
            Val::I32(v) => *v as u32,
            other => panic!("expected an address, got {other:?}"),
        }
    }

    fn write(&mut self, arg: usize, values: &[f64]) {
        let at = self.address(&format!("{}_arg_ptr", self.name), Some(arg as i32));
        let memory = self.instance.get_memory(&self.store, "memory").unwrap();
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        memory
            .write(&mut self.store, at as usize, &bytes)
            .expect("argument buffer is inside linear memory");
    }

    fn write_scalar(&mut self, arg: usize, value: f64) {
        self.write(arg, &[value]);
    }

    fn write_int(&mut self, arg: usize, value: i64) {
        let at = self.address(&format!("{}_arg_ptr", self.name), Some(arg as i32));
        let memory = self.instance.get_memory(&self.store, "memory").unwrap();
        memory
            .write(&mut self.store, at as usize, &value.to_le_bytes())
            .unwrap();
    }

    fn run(&mut self) -> Result<(), wasmi::Error> {
        let func = self.instance.get_func(&self.store, &self.name).unwrap();
        let mut out = [Val::I32(0)];
        func.call(&mut self.store, &[], &mut out)
    }

    fn read(&mut self, count: usize) -> Vec<f64> {
        let at = self.address(&format!("{}_ret_ptr", self.name), None);
        let memory = self.instance.get_memory(&self.store, "memory").unwrap();
        let mut bytes = vec![0u8; count * 8];
        memory.read(&self.store, at as usize, &mut bytes).unwrap();
        bytes
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    fn read_scalar(&mut self) -> f64 {
        self.read(1)[0]
    }
}

/// Compile, feed the tensor arguments in order, run, and read `count`
/// elements back.
fn evaluate(source: &str, name: &str, args: &[&[f64]], count: usize) -> Vec<f64> {
    let wasm = build(source);
    let mut driver = Driver::new(&wasm, name);
    for (i, values) in args.iter().enumerate() {
        driver.write(i, values);
    }
    driver.run().expect("the module must not trap");
    driver.read(count)
}

fn bits(values: &[f64]) -> Vec<u64> {
    values.iter().map(|v| v.to_bits()).collect()
}

fn nox3(v: [f64; 3]) -> Tensor<f64, Const<3>, ArrayRepr> {
    v.into()
}

fn nox_values<const N: usize>(t: Tensor<f64, Const<N>, ArrayRepr>) -> Vec<f64> {
    t.into_inner().view().buf().to_vec()
}

#[test]
fn a_tensor_signature_moves_the_whole_function_into_memory() {
    let module =
        compile("def scale(v: Tensor[f64, 3], k: f64) -> Tensor[f64, 3]:\n    return v * k\n")
            .unwrap();
    let sig = &module.manifest.functions[0];
    assert!(sig.uses_buffers());
    assert_eq!(
        sig.params[0].1,
        Ty::Tensor {
            dtype: Dtype::F64,
            shape: vec![3]
        }
    );
    assert_eq!(format!("{}", sig.ret), "Tensor[f64, 3]");

    // A scalar-only function next to it keeps passing by value.
    let module = compile(
        "def scale(v: Tensor[f64, 3]) -> f64:\n\
         \x20   return v[0]\n\
         def twice(x: f64) -> f64:\n\
         \x20   return x * 2.0\n",
    )
    .unwrap();
    assert!(module.manifest.functions[0].uses_buffers());
    assert!(!module.manifest.functions[1].uses_buffers());
}

#[test]
fn elementwise_arithmetic_agrees_with_nox() {
    let a = [1.5f64, -2.25, 3.75];
    let b = [0.5f64, 4.0, -1.25];

    for (op, oracle) in [
        ("+", (|x, y| x + y) as fn(f64, f64) -> f64),
        ("-", |x, y| x - y),
        ("*", |x, y| x * y),
        ("/", |x, y| x / y),
    ] {
        let source = format!(
            "def f(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> Tensor[f64, 3]:\n    return a {op} b\n"
        );
        let got = evaluate(&source, "f", &[&a, &b], 3);
        let want: Vec<f64> = a.iter().zip(&b).map(|(x, y)| oracle(*x, *y)).collect();
        assert_eq!(bits(&got), bits(&want), "a {op} b");
    }

    // The oracle for `+` and `*` is nox itself, elementwise.
    let got = evaluate(
        "def f(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> Tensor[f64, 3]:\n    return a * b + a\n",
        "f",
        &[&a, &b],
        3,
    );
    let want = nox_values(nox3(a) * nox3(b) + nox3(a));
    assert_eq!(bits(&got), bits(&want));
}

#[test]
fn a_scalar_broadcasts_against_a_tensor() {
    let v = [1.0f64, 2.0, 3.0];
    let got = evaluate(
        "def f(v: Tensor[f64, 3], k: f64) -> Tensor[f64, 3]:\n    return v * k + 1.0\n",
        "f",
        &[&v],
        3,
    );
    let _ = got;

    let wasm =
        build("def f(v: Tensor[f64, 3], k: f64) -> Tensor[f64, 3]:\n    return v * k + 1.0\n");
    let mut driver = Driver::new(&wasm, "f");
    driver.write(0, &v);
    driver.write_scalar(1, 2.5);
    driver.run().unwrap();
    let got = driver.read(3);

    let k = Tensor::<f64, Const<3>, ArrayRepr>::from([2.5, 2.5, 2.5]);
    let one = Tensor::<f64, Const<3>, ArrayRepr>::from([1.0, 1.0, 1.0]);
    let want = nox_values(nox3(v) * k + one);
    assert_eq!(bits(&got), bits(&want));
}

/// nox's rule, right-aligned: a `(3, 1)` column stretches across a `(3, 3)`.
#[test]
fn rank_two_broadcasts_right_aligned() {
    let m = [1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
    let row = [10.0f64, 20.0, 30.0];
    let got = evaluate(
        "def f(m: Tensor[f64, (3, 3)], r: Tensor[f64, (1, 3)]) -> Tensor[f64, (3, 3)]:\n\
         \x20   return m + r\n",
        "f",
        &[&m, &row],
        9,
    );
    let want: Vec<f64> = m.iter().enumerate().map(|(i, x)| x + row[i % 3]).collect();
    assert_eq!(bits(&got), bits(&want));

    for source in [
        "def f(a: Tensor[f64, 3], b: Tensor[f64, 4]) -> Tensor[f64, 3]:\n    return a + b\n",
        "def f(a: Tensor[f64, (2, 3)], b: Tensor[f64, (3, 2)]) -> Tensor[f64, (2, 3)]:\n    return a + b\n",
    ] {
        let diags = reject(source);
        assert!(format!("{diags}").contains("broadcast"));
    }
}

/// P1: small constant shapes open-code and reach no kernel at all; large ones
/// still call one. Both must compute the same thing, which is the point — the
/// choice is a size trade, not a semantic one.
#[test]
fn small_shapes_open_code_and_large_ones_call_kernels() {
    let source = |n: usize| {
        format!("def f(a: Tensor[f64, {n}], b: Tensor[f64, {n}]) -> f64:\n    return sum(a + b)\n")
    };
    let small = build(&source(3));
    let large = build(&source(200));
    assert!(
        !reaches_kernels(&small),
        "a length-3 add must be open-coded"
    );
    assert!(
        reaches_kernels(&large),
        "a length-200 add must call kernels"
    );

    for n in [3usize, 200] {
        let a: Vec<f64> = (0..n).map(|i| i as f64 * 0.25 - 1.0).collect();
        let b: Vec<f64> = (0..n).map(|i| 1.0 / (i as f64 + 3.0)).collect();
        let got = evaluate(&source(n), "f", &[&a, &b], 1);
        let want: f64 = a
            .iter()
            .zip(&b)
            .map(|(x, y)| x + y)
            .fold(0.0, |acc, v| acc + v);
        assert_eq!(got[0].to_bits(), want.to_bits(), "at length {n}");
    }
}

/// Whether any prelude kernel survived the call-graph walk into this module.
fn reaches_kernels(wasm: &[u8]) -> bool {
    wasmparser::Parser::new(0)
        .parse_all(wasm)
        .filter_map(|p| match p.unwrap() {
            wasmparser::Payload::ExportSection(r) => Some(r),
            _ => None,
        })
        .flatten()
        .any(|e| e.unwrap().name.starts_with("k_"))
}

#[test]
fn negation_and_powers_agree_with_nox() {
    let v = [1.5f64, -2.0, 0.5];
    let got = evaluate(
        "def f(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n    return -(v ** 3)\n",
        "f",
        &[&v],
        3,
    );
    let want = nox_values(-(nox3(v) * nox3(v) * nox3(v)));
    assert_eq!(bits(&got), bits(&want));
}

#[test]
fn indexing_reads_constant_and_variable_positions() {
    let v = [10.0f64, 20.0, 30.0];
    let wasm = build("def pick(v: Tensor[f64, 3], i: i64) -> f64:\n    return v[i]\n");
    for (i, want) in v.iter().enumerate() {
        let mut driver = Driver::new(&wasm, "pick");
        driver.write(0, &v);
        driver.write_int(1, i as i64);
        driver.run().unwrap();
        assert_eq!(driver.read_scalar(), *want);
    }

    let wasm = build("def first(v: Tensor[f64, 3]) -> f64:\n    return v[0] + v[2]\n");
    let mut driver = Driver::new(&wasm, "first");
    driver.write(0, &v);
    driver.run().unwrap();
    assert_eq!(driver.read_scalar(), 40.0);

    // Rank two takes two indices, row-major.
    let m = [1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0];
    let wasm = build("def at(m: Tensor[f64, (2, 3)]) -> f64:\n    return m[1, 2]\n");
    let mut driver = Driver::new(&wasm, "at");
    driver.write(0, &m);
    driver.run().unwrap();
    assert_eq!(driver.read_scalar(), 6.0);
}

#[test]
fn an_out_of_range_index_traps_rather_than_reading_past_the_end() {
    let wasm = build("def pick(v: Tensor[f64, 3], i: i64) -> f64:\n    return v[i]\n");
    for bad in [3i64, 4, -1, i64::MIN] {
        let mut driver = Driver::new(&wasm, "pick");
        driver.write(0, &[1.0, 2.0, 3.0]);
        driver.write_int(1, bad);
        assert!(driver.run().is_err(), "index {bad} must trap");
    }

    // A constant index out of range never gets that far.
    let diags = reject("def f(v: Tensor[f64, 3]) -> f64:\n    return v[3]\n");
    assert!(format!("{diags}").contains("outside 0..3"));
}

#[test]
fn element_assignment_builds_a_result() {
    let v = [1.0f64, 2.0, 3.0];
    let got = evaluate(
        "def reverse(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n\
         \x20   out: Tensor[f64, 3] = v * 1.0\n\
         \x20   for i in range(3):\n\
         \x20       out[i] = v[2 - i]\n\
         \x20   return out\n",
        "reverse",
        &[&v],
        3,
    );
    assert_eq!(got, vec![3.0, 2.0, 1.0]);
}

#[test]
fn for_range_drives_a_reduction() {
    let v = [1.5f64, 2.25, -3.0];
    let wasm = build(
        "def total(v: Tensor[f64, 3]) -> f64:\n\
         \x20   acc = 0.0\n\
         \x20   for i in range(3):\n\
         \x20       acc += v[i]\n\
         \x20   return acc\n",
    );
    let mut driver = Driver::new(&wasm, "total");
    driver.write(0, &v);
    driver.run().unwrap();
    assert_eq!(driver.read_scalar(), (1.5 + 2.25) + -3.0);
}

#[test]
fn range_variants_count_up_and_down() {
    let wasm = build(
        "def up(stop: i64) -> i64:\n\
         \x20   acc = 0\n\
         \x20   for i in range(stop):\n\
         \x20       acc += i\n\
         \x20   return acc\n\
         def span(lo: i64, hi: i64) -> i64:\n\
         \x20   acc = 0\n\
         \x20   for i in range(lo, hi):\n\
         \x20       acc += i\n\
         \x20   return acc\n\
         def down(lo: i64, hi: i64) -> i64:\n\
         \x20   acc = 0\n\
         \x20   for i in range(hi, lo, -1):\n\
         \x20       acc += i\n\
         \x20   return acc\n",
    );
    assert_eq!(super::run_i64(&wasm, "up", &[super::iv(5)]), 1 + 2 + 3 + 4);
    assert_eq!(super::run_i64(&wasm, "up", &[super::iv(0)]), 0);
    assert_eq!(
        super::run_i64(&wasm, "span", &[super::iv(2), super::iv(6)]),
        2 + 3 + 4 + 5
    );
    assert_eq!(
        super::run_i64(&wasm, "down", &[super::iv(0), super::iv(4)]),
        4 + 3 + 2 + 1
    );
}

#[test]
fn dot_and_sum_agree_with_nox() {
    let a = [1.5f64, -2.25, 3.75];
    let b = [0.5f64, 4.0, -1.25];

    let wasm =
        build("def inner(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> f64:\n    return dot(a, b)\n");
    let mut driver = Driver::new(&wasm, "inner");
    driver.write(0, &a);
    driver.write(1, &b);
    driver.run().unwrap();
    let want = nox3(a).dot(&nox3(b)).into_inner().view().buf()[0];
    assert_eq!(driver.read_scalar().to_bits(), want.to_bits());

    let wasm = build("def total(v: Tensor[f64, 3]) -> f64:\n    return sum(v)\n");
    let mut driver = Driver::new(&wasm, "total");
    driver.write(0, &a);
    driver.run().unwrap();
    assert_eq!(driver.read_scalar(), (a[0] + a[1]) + a[2]);

    let wasm = build("def size(v: Tensor[f64, 3]) -> i64:\n    return len(v)\n");
    let mut driver = Driver::new(&wasm, "size");
    driver.write(0, &a);
    driver.run().unwrap();
    let at = driver.address("size_ret_ptr", None);
    let memory = driver.instance.get_memory(&driver.store, "memory").unwrap();
    let mut bytes = [0u8; 8];
    memory.read(&driver.store, at as usize, &mut bytes).unwrap();
    assert_eq!(i64::from_le_bytes(bytes), 3);
}

#[test]
fn matrix_products_agree_with_nox() {
    let m = [1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0];
    let v = [1.0f64, 0.5, -2.0];
    let got = evaluate(
        "def mv(m: Tensor[f64, (2, 3)], v: Tensor[f64, 3]) -> Tensor[f64, 2]:\n\
         \x20   return dot(m, v)\n",
        "mv",
        &[&m, &v],
        2,
    );
    let nm: Tensor<f64, (Const<2>, Const<3>), ArrayRepr> =
        [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]].into();
    let nv: Tensor<f64, Const<3>, ArrayRepr> = v.into();
    let want = nm.dot(&nv).into_inner().view().buf().to_vec();
    assert_eq!(bits(&got), bits(&want));

    let a = [1.0f64, 2.0, 3.0, 4.0];
    let b = [5.0f64, 6.0, 7.0, 8.0];
    let got = evaluate(
        "def mm(a: Tensor[f64, (2, 2)], b: Tensor[f64, (2, 2)]) -> Tensor[f64, (2, 2)]:\n\
         \x20   return dot(a, b)\n",
        "mm",
        &[&a, &b],
        4,
    );
    let na: Tensor<f64, (Const<2>, Const<2>), ArrayRepr> = [[1.0, 2.0], [3.0, 4.0]].into();
    let nb: Tensor<f64, (Const<2>, Const<2>), ArrayRepr> = [[5.0, 6.0], [7.0, 8.0]].into();
    let want = na.dot(&nb).into_inner().view().buf().to_vec();
    assert_eq!(bits(&got), bits(&want));
}

#[test]
fn tensors_cross_between_functions_through_the_same_buffers() {
    let v = [3.0f64, 4.0, 12.0];
    let got = evaluate(
        "def double(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n\
         \x20   return v * 2.0\n\
         def quadruple(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n\
         \x20   return double(double(v))\n",
        "quadruple",
        &[&v],
        3,
    );
    assert_eq!(got, vec![12.0, 16.0, 48.0]);

    // A scalar result of a buffer-ABI callee comes back through its buffer.
    let wasm = build(
        "def norm(v: Tensor[f64, 3]) -> f64:\n\
         \x20   return sqrt(dot(v, v))\n\
         def twice_norm(v: Tensor[f64, 3]) -> f64:\n\
         \x20   return norm(v) + norm(v)\n",
    );
    let mut driver = Driver::new(&wasm, "twice_norm");
    driver.write(0, &v);
    driver.run().unwrap();
    let want = libm::sqrt(3.0f64 * 3.0 + 4.0 * 4.0 + 12.0 * 12.0);
    assert_eq!(driver.read_scalar(), want + want);
}

/// Static buffers cannot survive a second activation, so the cycle is refused
/// rather than quietly corrupted.
#[test]
fn tensor_recursion_is_refused() {
    for source in [
        "def f(v: Tensor[f64, 3], n: i64) -> Tensor[f64, 3]:\n\
         \x20   if n <= 0:\n\
         \x20       return v\n\
         \x20   else:\n\
         \x20       return f(v * 2.0, n - 1)\n",
        "def a(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n\
         \x20   return b(v)\n\
         def b(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n\
         \x20   return a(v)\n",
    ] {
        let diags = reject(source);
        assert!(
            format!("{diags}").contains("recursive"),
            "expected a recursion diagnostic, got:\n{diags}"
        );
    }

    // Scalar recursion is untouched: it lives in wasm locals.
    let wasm = build(
        "def fact(n: i64) -> i64:\n\
         \x20   if n <= 1:\n\
         \x20       return 1\n\
         \x20   else:\n\
         \x20       return n * fact(n - 1)\n",
    );
    assert_eq!(super::run_i64(&wasm, "fact", &[super::iv(6)]), 720);
}

#[test]
fn a_realistic_tensor_program_agrees_with_nox() {
    let omega = [0.01f64, -0.02, 0.005];
    let gain = [0.5f64, 0.5, 0.5];
    let wasm = build(
        "def rate(omega_b: Tensor[f64, 3]) -> f64:\n\
         \x20   return sqrt(dot(omega_b, omega_b))\n\
         def control(omega_b: Tensor[f64, 3], k: Tensor[f64, 3]) -> Tensor[f64, 3]:\n\
         \x20   return -(k * omega_b) * rate(omega_b)\n",
    );
    let mut driver = Driver::new(&wasm, "control");
    driver.write(0, &omega);
    driver.write(1, &gain);
    driver.run().unwrap();
    let got = driver.read(3);

    // Elementwise stays nox's; the contraction uses the language's own
    // non-fused definition, for the reason `a_contraction_is_not_fused` pins.
    let no = nox3(omega);
    let nk = nox3(gain);
    let magnitude = libm::sqrt(sequential_dot(&omega, &omega));
    let spread = Tensor::<f64, Const<3>, ArrayRepr>::from([magnitude; 3]);
    let want = nox_values(-(nk * no) * spread);
    assert_eq!(bits(&got), bits(&want));
}

fn sequential_dot(a: &[f64], b: &[f64]) -> f64 {
    let mut acc = 0.0;
    for (x, y) in a.iter().zip(b) {
        acc += x * y;
    }
    acc
}

/// `dot` is a sequential sum, not a fused one — nox's reaches faer's FMA and
/// core wasm has no FMA instruction to answer with. Pinned so the divergence
/// stays a known fact.
#[test]
fn a_contraction_is_not_fused() {
    let v = [0.01f64, -0.02, 0.005];
    let wasm = build("def n2(v: Tensor[f64, 3]) -> f64:\n    return dot(v, v)\n");
    let mut driver = Driver::new(&wasm, "n2");
    driver.write(0, &v);
    driver.run().unwrap();
    let compiled = driver.read_scalar();

    assert_eq!(compiled.to_bits(), sequential_dot(&v, &v).to_bits());

    let fused = nox3(v).dot(&nox3(v)).into_inner().view().buf()[0];
    assert_ne!(
        compiled.to_bits(),
        fused.to_bits(),
        "if nox stopped fusing, this note and the harness docs are stale"
    );
    assert_eq!(
        fused.to_bits(),
        v[0].mul_add(v[0], v[1].mul_add(v[1], v[2] * v[2]))
            .to_bits()
    );
}

#[test]
fn tensor_rejections_carry_spans() {
    for (source, needle) in [
        (
            "def f(v: Tensor[f64, 0]) -> f64:\n    return v[0]\n",
            "positive constant",
        ),
        (
            "def f(v: Tensor[i64, 3]) -> f64:\n    return v[0]\n",
            "Tensor[f64",
        ),
        (
            "def f(v: Tensor[f64]) -> f64:\n    return v[0]\n",
            "Tensor[f64, N]",
        ),
        (
            "def f(x: f64) -> f64:\n    return x[0]\n",
            "cannot be indexed",
        ),
        (
            "def f(m: Tensor[f64, (2, 3)]) -> f64:\n    return m[0]\n",
            "needs 2 indices",
        ),
        (
            "def f(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> bool:\n    return a == b\n",
            "do not compare",
        ),
        (
            "def f(a: Tensor[f64, 3], b: Tensor[f64, 3]) -> Tensor[f64, 3]:\n    return a % b\n",
            "tensors support",
        ),
        (
            "def f(a: Tensor[f64, 3], b: Tensor[f64, 2]) -> f64:\n    return dot(a, b)\n",
            "do not contract",
        ),
        (
            "def f(x: f64) -> f64:\n    return sum(x)\n",
            "needs a tensor",
        ),
    ] {
        let diags = reject(source);
        assert!(
            format!("{diags}").contains(needle),
            "expected {source:?} to mention {needle:?}, got:\n{diags}"
        );
    }
}

#[test]
fn tensor_modules_stay_closed_and_within_their_memory() {
    let module = compile(
        "def f(a: Tensor[f64, (3, 3)], b: Tensor[f64, (3, 3)]) -> Tensor[f64, (3, 3)]:\n\
         \x20   return dot(a, b) + a * b\n",
    )
    .unwrap();
    assert_eq!(super::imports(&module.wasm), 0);
    let mut driver = Driver::new(&module.wasm, "f");
    driver.write(0, &[1.0; 9]);
    driver.write(1, &[2.0; 9]);
    driver.run().unwrap();
    assert_eq!(driver.read(9), vec![8.0; 9]);
}

/// Right-aligned broadcast, computed the obvious way, as the reference every
/// rank combination is checked against.
fn broadcast_reference(a: &[f64], sa: &[usize], b: &[f64], sb: &[usize], out: &[usize]) -> Vec<f64> {
    let rank = out.len();
    let pad = |s: &[usize]| {
        let mut v = vec![1usize; rank - s.len()];
        v.extend_from_slice(s);
        v
    };
    let (pa, pb) = (pad(sa), pad(sb));
    let strides = |s: &[usize]| {
        let mut r = vec![0usize; rank];
        let mut acc = 1;
        for i in (0..rank).rev() {
            r[i] = if s[i] == 1 { 0 } else { acc };
            acc *= s[i];
        }
        r
    };
    let (ta, tb) = (strides(&pa), strides(&pb));
    let mut index = vec![0usize; rank];
    let mut result = Vec::new();
    for _ in 0..out.iter().product::<usize>() {
        let (mut ia, mut ib) = (0, 0);
        for axis in 0..rank {
            ia += index[axis] * ta[axis];
            ib += index[axis] * tb[axis];
        }
        result.push(a[ia] + b[ib]);
        for axis in (0..rank).rev() {
            index[axis] += 1;
            if index[axis] < out[axis] {
                break;
            }
            index[axis] = 0;
        }
    }
    result
}

fn annotation_of(shape: &[usize]) -> String {
    match shape {
        [] => "f64".to_string(),
        [n] => format!("Tensor[f64, {n}]"),
        dims => format!(
            "Tensor[f64, ({})]",
            dims.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Every rank combination the type system can express, through *both* emit
/// paths — the shapes below `OPEN_CODE_MAX_OPS` are open-coded and the ones
/// above call the broadcasting kernel, and the two must agree with each other
/// and with the rule.
///
/// The user-facing case is the first line: a channel carrying `[1, 2, 3]`,
/// plus one, is `[2, 3, 4]`.
#[test]
fn broadcast_holds_across_every_rank_combination() {
    let combinations: &[(&[usize], &[usize], &[usize])] = &[
        // Scalar against a vector, both ways round: the one-liner's case.
        (&[], &[3], &[3]),
        (&[3], &[], &[3]),
        (&[3], &[3], &[3]),
        // Ranks that differ, right-aligned.
        (&[3], &[2, 3], &[2, 3]),
        (&[2, 3], &[3], &[2, 3]),
        (&[], &[2, 3], &[2, 3]),
        // An extent of one stretches.
        (&[1, 3], &[2, 3], &[2, 3]),
        (&[2, 1], &[2, 3], &[2, 3]),
        (&[2, 3], &[2, 3], &[2, 3]),
        // Rank three, padded on the left the same way.
        (&[3, 3], &[2, 3, 3], &[2, 3, 3]),
        (&[2, 3, 3], &[3], &[2, 3, 3]),
        // Past the open-coding threshold, so the kernel answers instead.
        (&[200], &[], &[200]),
        (&[200], &[200], &[200]),
        (&[2, 200], &[200], &[2, 200]),
        (&[200], &[2, 200], &[2, 200]),
        (&[], &[2, 200], &[2, 200]),
    ];

    for (sa, sb, out) in combinations {
        let source = format!(
            "def f(a: {}, b: {}) -> {}:\n    return a + b\n",
            annotation_of(sa),
            annotation_of(sb),
            annotation_of(out),
        );
        let count: usize = out.iter().product();
        let a: Vec<f64> = (0..sa.iter().product::<usize>().max(1))
            .map(|i| i as f64 + 1.0)
            .collect();
        let b: Vec<f64> = (0..sb.iter().product::<usize>().max(1))
            .map(|i| (i as f64 + 1.0) * 10.0)
            .collect();

        let got = evaluate(&source, "f", &[&a, &b], count);
        let want = broadcast_reference(&a, sa, &b, sb, out);
        assert_eq!(bits(&got), bits(&want), "{sa:?} + {sb:?} -> {out:?}");
    }
}

/// The exact case from the field, end to end: `[1, 2, 3] + 1.0`.
#[test]
fn a_vector_plus_a_scalar_is_the_vector_shifted() {
    let v = [1.0f64, 2.0, 3.0];
    let got = evaluate(
        "def f(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n    return v + 1.0\n",
        "f",
        &[&v],
        3,
    );
    assert_eq!(got, vec![2.0, 3.0, 4.0]);

    // The scalar on the left broadcasts the same way, and `nox` agrees.
    let got = evaluate(
        "def f(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n    return 1.0 + v\n",
        "f",
        &[&v],
        3,
    );
    let want = nox_values(nox3(v) + nox3([1.0; 3]));
    assert_eq!(bits(&got), bits(&want));
}

/// Every arithmetic operator broadcasts, not just `+`.
#[test]
fn the_whole_arithmetic_set_broadcasts() {
    let v = [1.0f64, 2.0, 4.0];
    for (op, want) in [
        ("+", [3.0, 4.0, 6.0]),
        ("-", [-1.0, 0.0, 2.0]),
        ("*", [2.0, 4.0, 8.0]),
        ("/", [0.5, 1.0, 2.0]),
    ] {
        let got = evaluate(
            &format!("def f(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n    return v {op} 2.0\n"),
            "f",
            &[&v],
            3,
        );
        assert_eq!(got, want.to_vec(), "`{op}` did not broadcast");
    }

    // Unary negation over a tensor is unaffected by any of this.
    let got = evaluate(
        "def f(v: Tensor[f64, 3]) -> Tensor[f64, 3]:\n    return -v\n",
        "f",
        &[&v],
        3,
    );
    assert_eq!(got, vec![-1.0, -2.0, -4.0]);
}

/// Shapes that do not broadcast are refused with both of them named, at every
/// rank — the rule is nox's, and so is what it rejects.
#[test]
fn shapes_that_do_not_broadcast_are_refused_with_spans() {
    for (a, b) in [
        (&[3usize][..], &[4usize][..]),
        (&[2, 3][..], &[3, 2][..]),
        (&[2, 3][..], &[4][..]),
        (&[2, 3, 3][..], &[2, 2, 3][..]),
    ] {
        let source = format!(
            "def f(a: {}, b: {}) -> f64:\n    return sum(a + b)\n",
            annotation_of(a),
            annotation_of(b),
        );
        let diags = reject(&source);
        let text = format!("{diags}");
        assert!(text.contains("broadcast"), "{a:?} vs {b:?}: {text}");
        assert!(
            text.contains(&format!("{a:?}")) && text.contains(&format!("{b:?}")),
            "the diagnostic must name both shapes: {text}"
        );
    }
}
