//! M2: the ground rules, one test each, checked against nox.
//!
//! These are the decisions the phase ratifies. Each is pinned here so that
//! changing one is a deliberate act with a failing test attached, and each
//! that has a numeric answer is checked against the oracle in
//! [`differential`](super::differential) rather than against a literal the
//! test author typed.

use super::differential::{agrees, agrees_over, nox_scalar, nox_value, py_floordiv, py_rem};
use super::{build, eval_bool, eval_f64, eval_i64, fv, iv, reject, run_f64, run_i64, trap};

/// Rule 1 — ints are `i64` and wrap. The divergence from CPython is bignums,
/// and it is documented rather than papered over.
#[test]
fn ints_are_i64_and_wrap() {
    assert_eq!(eval_i64("9223372036854775807"), i64::MAX);
    assert_eq!(eval_i64("-9223372036854775807 - 1"), i64::MIN);

    let wasm = build("def add(a: i64, b: i64) -> i64:\n    return a + b\n");
    assert_eq!(
        run_i64(&wasm, "add", &[iv(i64::MAX), iv(1)]),
        i64::MAX.wrapping_add(1)
    );
    let wasm = build("def mul(a: i64, b: i64) -> i64:\n    return a * b\n");
    assert_eq!(
        run_i64(&wasm, "mul", &[iv(i64::MAX), iv(3)]),
        i64::MAX.wrapping_mul(3)
    );

    let diags = reject("def f() -> i64:\n    return 99999999999999999999999999\n");
    assert!(format!("{diags}").contains("bignums"));
}

/// Rule 2 — `/` is true division and always yields `f64`, `int / int`
/// included.
#[test]
fn division_always_yields_f64() {
    agrees("7.0 / 2.0", || {
        nox_value(nox_scalar(7.0) / nox_scalar(2.0))
    });
    assert_eq!(eval_f64("7 / 2"), 3.5);
    assert_eq!(eval_f64("4 / 2"), 2.0);

    // The result is f64, so it cannot be returned as i64 without narrowing.
    let diags = reject("def f() -> i64:\n    return 4 / 2\n");
    assert!(format!("{diags}").contains("int(x)"));
}

/// Rule 3 — `//` and `%` floor, so the sign of the result follows the divisor.
/// wasm's `rem` truncates, so this is a real correction, not a rename.
#[test]
fn floor_division_and_modulo_follow_the_divisor() {
    let cases = [(7i64, 2i64), (-7, 2), (7, -2), (-7, -2), (6, 3), (-6, 3)];

    let div = build("def f(a: i64, b: i64) -> i64:\n    return a // b\n");
    let rem = build("def f(a: i64, b: i64) -> i64:\n    return a % b\n");
    for (a, b) in cases {
        assert_eq!(
            run_i64(&div, "f", &[iv(a), iv(b)]),
            py_floordiv(a, b),
            "{a} // {b}"
        );
        assert_eq!(run_i64(&rem, "f", &[iv(a), iv(b)]), py_rem(a, b), "{a} % {b}");
    }

    assert_eq!(eval_i64("-7 // 2"), -4);
    assert_eq!(eval_i64("7 // -2"), -4);
    assert_eq!(eval_i64("-7 % 2"), 1);
    assert_eq!(eval_i64("7 % -2"), -1);

    // Floats floor the same way.
    agrees_over(
        "def f(a: f64, b: f64) -> f64:\n    return a // b\n",
        "f",
        &[&[7.0, 2.0], &[-7.0, 2.0], &[7.0, -2.0], &[-7.5, 2.0]],
        |args| libm::floor(args[0] / args[1]),
    );
    agrees_over(
        "def f(a: f64, b: f64) -> f64:\n    return a % b\n",
        "f",
        &[&[7.0, 2.0], &[-7.0, 2.0], &[7.0, -2.0], &[-7.5, 2.0]],
        |args| {
            let r = libm::fmod(args[0], args[1]);
            if r != 0.0 && (r < 0.0) != (args[1] < 0.0) {
                r + args[1]
            } else {
                r
            }
        },
    );
}

/// Rule 4 — integer division and modulo by zero trap. Float division by zero
/// is IEEE-754, which has an answer, so it gets one.
#[test]
fn integer_division_by_zero_traps_and_float_division_does_not() {
    let div = build("def f(a: i64, b: i64) -> i64:\n    return a // b\n");
    assert!(trap(&div, "f", &[iv(1), iv(0)]).contains("divide by zero"));
    let rem = build("def f(a: i64, b: i64) -> i64:\n    return a % b\n");
    assert!(trap(&rem, "f", &[iv(1), iv(0)]).contains("divide by zero"));

    let float = build("def f(a: f64, b: f64) -> f64:\n    return a / b\n");
    assert_eq!(run_f64(&float, "f", &[fv(1.0), fv(0.0)]), f64::INFINITY);
    assert_eq!(run_f64(&float, "f", &[fv(-1.0), fv(0.0)]), f64::NEG_INFINITY);
    assert!(run_f64(&float, "f", &[fv(0.0), fv(0.0)]).is_nan());
}

/// Rule 5 — `**` is repeated multiplication for a small non-negative integer
/// literal exponent, and `pow` from the prelude otherwise.
#[test]
fn exponentiation_unrolls_small_literal_exponents() {
    assert_eq!(eval_i64("2 ** 10"), 1024);
    assert_eq!(eval_i64("2 ** 0"), 1);
    assert_eq!(eval_i64("2 ** 1"), 2);
    assert_eq!(eval_i64("(-2) ** 3"), -8);
    agrees("1.5 ** 3", || {
        let x = nox_scalar(1.5);
        nox_value(x * x * x)
    });

    // A literal exponent reaches no kernel; anything else reaches `pow`.
    let unrolled = build("def f(x: f64) -> f64:\n    return x ** 2\n");
    let called = build("def f(x: f64, y: f64) -> f64:\n    return x ** y\n");
    assert!(
        unrolled.len() < called.len(),
        "a literal exponent should not drag `pow` in"
    );
    agrees("2.0 ** 0.5", || libm::pow(2.0, 0.5));

    // A negative exponent is not repeated multiplication, so it promotes.
    assert_eq!(eval_f64("2 ** -1"), libm::pow(2.0, -1.0));
}

/// Rule 6 — `bool` is not an int in disguise.
#[test]
fn bool_is_not_an_int() {
    for source in [
        "def f() -> i64:\n    return True + 1\n",
        "def f() -> i64:\n    return True\n",
        "def f(x: f64) -> f64:\n    if x:\n        return 1.0\n    return 0.0\n",
        "def f(n: i64) -> bool:\n    return n and True\n",
        "def f() -> bool:\n    return not 1\n",
        "def f() -> bool:\n    return True < False\n",
    ] {
        reject(source);
    }

    assert!(eval_bool("True == True"));
    assert!(eval_bool("True != False"));
    assert!(eval_bool("(1 < 2) == (3 < 4)"));

    // A condition must be a comparison, and a comparison is what it yields.
    let wasm = build("def f(x: f64) -> bool:\n    return x != 0.0\n");
    assert!(!super::run_bool(&wasm, "f", &[fv(0.0)]));
    assert!(super::run_bool(&wasm, "f", &[fv(1.0)]));
}

/// Rule 7 — mixed arithmetic promotes `i64` to `f64`; narrowing is explicit
/// and truncates toward zero.
#[test]
fn promotion_is_implicit_and_narrowing_is_not() {
    agrees("1 + 2.0", || {
        nox_value(nox_scalar(1.0) + nox_scalar(2.0))
    });
    assert_eq!(eval_f64("2 * 1.5"), 3.0);
    assert_eq!(eval_f64("float(3)"), 3.0);

    assert_eq!(eval_i64("int(2.7)"), 2);
    assert_eq!(eval_i64("int(-2.7)"), -2);
    assert_eq!(eval_i64("int(3)"), 3);

    for source in [
        "def f(x: f64) -> i64:\n    return x\n",
        "def f(x: f64) -> i64:\n    n: i64 = x\n    return n\n",
    ] {
        let diags = reject(source);
        assert!(format!("{diags}").contains("int(x)"));
    }

    // Narrowing out of range is a trap, not a wrapped value.
    let wasm = build("def f(x: f64) -> i64:\n    return int(x)\n");
    assert!(!trap(&wasm, "f", &[fv(1e300)]).is_empty());
}

/// Rule 8 — the subset is small on purpose, and every refusal carries a span.
#[test]
fn everything_outside_the_subset_is_refused_with_a_span() {
    let source = "def f() -> f64:\n    return [1.0]\n";
    let diags = reject(source);
    let d = diags.iter().next().unwrap();
    assert_eq!(&source[d.span.start as usize..d.span.end as usize], "[1.0]");
}

/// The rules compose: a real expression checked end to end against nox.
#[test]
fn a_realistic_expression_agrees_with_nox() {
    agrees_over(
        "def norm(x: f64, y: f64, z: f64) -> f64:\n\
         \x20   return sqrt(x ** 2 + y ** 2 + z ** 2)\n",
        "norm",
        &[&[3.0, 4.0, 12.0], &[0.1, -0.2, 0.3], &[0.0, 0.0, 0.0]],
        |a| {
            let (x, y, z) = (nox_scalar(a[0]), nox_scalar(a[1]), nox_scalar(a[2]));
            nox_value((x * x + y * y + z * z).sqrt())
        },
    );

    agrees_over(
        "def lowpass(x: f64, prev: f64, k: f64) -> f64:\n\
         \x20   return k * x + (1.0 - k) * prev\n",
        "lowpass",
        &[&[1.0, 0.0, 0.2], &[-3.5, 2.25, 0.75], &[1e-9, 1e9, 0.5]],
        |a| {
            let (x, prev, k) = (nox_scalar(a[0]), nox_scalar(a[1]), nox_scalar(a[2]));
            nox_value(k * x + (nox_scalar(1.0) - k) * prev)
        },
    );

    agrees_over(
        "def bearing(y: f64, x: f64) -> f64:\n\
         \x20   return atan2(y, x)\n",
        "bearing",
        &[&[1.0, 1.0], &[-1.0, 1.0], &[1.0, -1.0], &[0.0, -1.0]],
        |a| nox_value(nox_scalar(a[0]).atan2(&nox_scalar(a[1]))),
    );
}

/// Control flow agrees too, not just expressions.
#[test]
fn an_iterative_program_agrees_with_nox() {
    agrees_over(
        "def decay(x0: f64, k: f64, steps: f64) -> f64:\n\
         \x20   x = x0\n\
         \x20   i = 0\n\
         \x20   while i < steps:\n\
         \x20       x = x * k\n\
         \x20       i += 1\n\
         \x20   return x\n",
        "decay",
        &[&[1.0, 0.5, 10.0], &[3.25, 0.9, 32.0], &[1.0, 1.0, 0.0]],
        |a| {
            let mut x = nox_scalar(a[0]);
            let k = nox_scalar(a[1]);
            for _ in 0..(a[2] as i64) {
                x = x * k;
            }
            nox_value(x)
        },
    );
}
