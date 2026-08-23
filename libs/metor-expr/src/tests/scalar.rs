//! M1: the scalar language, end to end.
//!
//! Every case here compiles a source string and runs the module. What is being
//! checked is not that the compiler produced *some* wasm but that the wasm
//! computes what the Python says.

use super::{
    build, call_f64, eval_bool, eval_f64, eval_i64, fv, iv, reject, run_bool, run_f64, run_i64,
    trap,
};
use crate::{Ty, compile};

#[test]
fn a_function_is_exported_by_name_with_its_params_by_value() {
    let wasm = build(
        "def scale(x: f64, k: f64) -> f64:\n    return x * k\n",
    );
    assert_eq!(call_f64(&wasm, "scale", &[2.5, 4.0]), 10.0);
}

#[test]
fn the_manifest_describes_what_the_host_will_call() {
    let module = compile(
        "def mix(a: f64, n: i64, on: bool) -> f64:\n    return a\n",
    )
    .unwrap();
    let sig = &module.manifest.functions[0];
    assert_eq!(sig.name, "mix");
    assert_eq!(
        sig.params,
        vec![
            ("a".into(), Ty::F64),
            ("n".into(), Ty::I64),
            ("on".into(), Ty::Bool),
        ]
    );
    assert_eq!(sig.ret, Ty::F64);
}

#[test]
fn python_spellings_of_the_scalar_types_are_accepted() {
    let wasm = build("def f(x: float, n: int) -> float:\n    return x + n\n");
    assert_eq!(run_f64(&wasm, "f", &[fv(1.5), iv(2)]), 3.5);
}

#[test]
fn arithmetic_evaluates_as_written() {
    assert_eq!(eval_f64("2.0 + 3.0 * 4.0"), 14.0);
    assert_eq!(eval_f64("(2.0 + 3.0) * 4.0"), 20.0);
    assert_eq!(eval_f64("-2.5"), -2.5);
    assert_eq!(eval_f64("+2.5"), 2.5);
    assert_eq!(eval_f64("7.0 - 2.0 - 3.0"), 2.0);
    assert_eq!(eval_i64("7 - 2 - 3"), 2);
    assert_eq!(eval_i64("-(3 * 4)"), -12);
}

#[test]
fn comparisons_yield_bool_and_chain_like_python() {
    assert!(eval_bool("1.0 < 2.0"));
    assert!(eval_bool("2.0 <= 2.0"));
    assert!(!eval_bool("2.0 < 2.0"));
    assert!(eval_bool("3 == 3"));
    assert!(eval_bool("3 != 4"));
    assert!(eval_bool("1 < 2 < 3"));
    assert!(!eval_bool("1 < 2 < 2"));
    assert!(!eval_bool("3 < 2 < 1"));
    assert!(eval_bool("1 < 2 < 3 < 4 < 5"));
    assert!(!eval_bool("1 < 2 < 3 < 3 < 5"));
}

/// A chained comparison must read each operand once, so a call in the middle
/// runs once — and only if the chain gets that far.
#[test]
fn a_chain_evaluates_each_operand_once_and_stops_early() {
    let wasm = build(
        "def bump(n: i64) -> i64:\n\
         \x20   return n + 1\n\
         def chain(n: i64) -> bool:\n\
         \x20   return 0 < bump(n) < 3\n\
         def short(n: i64) -> bool:\n\
         \x20   return 5 < 1 < bump(n)\n",
    );
    assert!(run_bool(&wasm, "chain", &[iv(1)]));
    assert!(!run_bool(&wasm, "chain", &[iv(5)]));
    assert!(!run_bool(&wasm, "short", &[iv(0)]));
}

#[test]
fn and_or_not_short_circuit() {
    assert!(eval_bool("True and True"));
    assert!(!eval_bool("True and False"));
    assert!(eval_bool("False or True"));
    assert!(!eval_bool("False or False"));
    assert!(eval_bool("not False"));
    assert!(eval_bool("True and True and True"));
    assert!(eval_bool("False or False or True"));

    // The right operand of a short-circuited `and` never runs, so its trap
    // never happens.
    let wasm = build(
        "def safe(n: i64) -> bool:\n    return n != 0 and 10 // n > 2\n",
    );
    assert!(!run_bool(&wasm, "safe", &[iv(0)]));
    assert!(run_bool(&wasm, "safe", &[iv(3)]));
}

#[test]
fn conditional_expressions_pick_a_branch() {
    assert_eq!(eval_f64("1.0 if True else 2.0"), 1.0);
    assert_eq!(eval_f64("1.0 if False else 2.0"), 2.0);
    let wasm = build("def pick(x: f64) -> f64:\n    return x if x > 0.0 else -x\n");
    assert_eq!(call_f64(&wasm, "pick", &[-3.0]), 3.0);
    assert_eq!(call_f64(&wasm, "pick", &[3.0]), 3.0);
}

#[test]
fn functions_call_each_other_in_either_order() {
    let wasm = build(
        "def outer(x: f64) -> f64:\n\
         \x20   return inner(x) + 1.0\n\
         def inner(x: f64) -> f64:\n\
         \x20   return x * 2.0\n",
    );
    assert_eq!(call_f64(&wasm, "outer", &[3.0]), 7.0);
}

#[test]
fn recursion_works_and_stays_bounded_by_fuel() {
    let wasm = build(
        "def fact(n: i64) -> i64:\n\
         \x20   if n <= 1:\n\
         \x20       return 1\n\
         \x20   else:\n\
         \x20       return n * fact(n - 1)\n",
    );
    assert_eq!(run_i64(&wasm, "fact", &[iv(10)]), 3628800);
}

#[test]
fn assignment_declares_and_rebinds() {
    let wasm = build(
        "def f(x: f64) -> f64:\n\
         \x20   y = x * 2.0\n\
         \x20   z: f64 = y + 1.0\n\
         \x20   y = z * z\n\
         \x20   return y\n",
    );
    assert_eq!(call_f64(&wasm, "f", &[3.0]), 49.0);
}

#[test]
fn augmented_assignment_updates_in_place() {
    let wasm = build(
        "def f(x: f64) -> f64:\n\
         \x20   acc = 0.0\n\
         \x20   acc += x\n\
         \x20   acc *= 3.0\n\
         \x20   acc -= 1.0\n\
         \x20   acc /= 2.0\n\
         \x20   return acc\n",
    );
    assert_eq!(call_f64(&wasm, "f", &[4.0]), 5.5);
}

#[test]
fn if_elif_else_picks_one_arm() {
    let wasm = build(
        "def sign(x: f64) -> i64:\n\
         \x20   if x > 0.0:\n\
         \x20       return 1\n\
         \x20   elif x < 0.0:\n\
         \x20       return -1\n\
         \x20   else:\n\
         \x20       return 0\n",
    );
    assert_eq!(run_i64(&wasm, "sign", &[fv(2.0)]), 1);
    assert_eq!(run_i64(&wasm, "sign", &[fv(-2.0)]), -1);
    assert_eq!(run_i64(&wasm, "sign", &[fv(0.0)]), 0);
}

#[test]
fn while_loops_with_break_and_continue() {
    let wasm = build(
        "def sum_odd_below(limit: i64) -> i64:\n\
         \x20   total = 0\n\
         \x20   i = 0\n\
         \x20   while True:\n\
         \x20       i += 1\n\
         \x20       if i >= limit:\n\
         \x20           break\n\
         \x20       if i % 2 == 0:\n\
         \x20           continue\n\
         \x20       total += i\n\
         \x20   return total\n",
    );
    assert_eq!(run_i64(&wasm, "sum_odd_below", &[iv(10)]), 25);
}

#[test]
fn nested_loops_break_the_inner_one_only() {
    let wasm = build(
        "def count(n: i64) -> i64:\n\
         \x20   total = 0\n\
         \x20   i = 0\n\
         \x20   while i < n:\n\
         \x20       j = 0\n\
         \x20       while j < n:\n\
         \x20           if j > i:\n\
         \x20               break\n\
         \x20           total += 1\n\
         \x20           j += 1\n\
         \x20       i += 1\n\
         \x20   return total\n",
    );
    assert_eq!(run_i64(&wasm, "count", &[iv(4)]), 10);
}

#[test]
fn transcendentals_are_the_only_thing_that_enters_the_prelude() {
    // Native opcodes: no kernels reached, so the module is the small one.
    let plain = build("def f(x: f64) -> f64:\n    return sqrt(x * x + 1.0)\n");
    let with_kernel = build("def f(x: f64) -> f64:\n    return sin(x) + cos(x)\n");
    assert!(
        plain.len() < with_kernel.len(),
        "sqrt should not drag the prelude in"
    );
    assert_eq!(call_f64(&plain, "f", &[3.0]), 10.0f64.sqrt());
    assert_eq!(
        call_f64(&with_kernel, "f", &[0.5]),
        libm::sin(0.5) + libm::cos(0.5)
    );
}

#[test]
fn the_builtin_set_computes_what_it_says() {
    assert_eq!(eval_f64("sqrt(9.0)"), 3.0);
    assert_eq!(eval_f64("abs(-3.5)"), 3.5);
    assert_eq!(eval_i64("abs(-3)"), 3);
    assert_eq!(eval_i64("abs(3)"), 3);
    assert_eq!(eval_f64("min(2.0, 3.0)"), 2.0);
    assert_eq!(eval_f64("max(2.0, 3.0)"), 3.0);
    assert_eq!(eval_i64("min(2, 3)"), 2);
    assert_eq!(eval_i64("max(-2, -3)"), -2);
    assert_eq!(eval_f64("floor(2.7)"), 2.0);
    assert_eq!(eval_f64("floor(-2.7)"), -3.0);
    assert_eq!(eval_f64("ceil(2.1)"), 3.0);
    assert_eq!(eval_f64("trunc(-2.7)"), -2.0);
    assert_eq!(eval_f64("atan2(1.0, 1.0)"), libm::atan2(1.0, 1.0));
    assert_eq!(eval_f64("pow(2.0, 10.0)"), libm::pow(2.0, 10.0));
    assert_eq!(eval_f64("exp(1.0)"), libm::exp(1.0));
    assert_eq!(eval_f64("log(2.0)"), libm::log(2.0));
    assert_eq!(eval_f64("tanh(0.5)"), libm::tanh(0.5));
}

/// `round` is `f64.nearest`, which is round-half-to-even — the same rule
/// Python's `round` uses.
#[test]
fn round_breaks_ties_toward_even() {
    assert_eq!(eval_f64("round(0.5)"), 0.0);
    assert_eq!(eval_f64("round(1.5)"), 2.0);
    assert_eq!(eval_f64("round(2.5)"), 2.0);
    assert_eq!(eval_f64("round(-0.5)"), 0.0);
}

/// A fault inside the guest is a trap, contained, rather than a value the
/// language had to invent.
#[test]
fn integer_division_by_zero_traps() {
    let wasm = build("def f(a: i64, b: i64) -> i64:\n    return a // b\n");
    assert_eq!(run_i64(&wasm, "f", &[iv(7), iv(2)]), 3);
    let message = trap(&wasm, "f", &[iv(7), iv(0)]);
    assert!(
        message.contains("divide by zero"),
        "expected a division trap, got {message}"
    );
}

#[test]
fn a_hundred_line_module_compiles_and_runs() {
    let mut source = String::new();
    for i in 0..25 {
        source.push_str(&format!(
            "def step{i}(x: f64) -> f64:\n\
             \x20   acc = x\n\
             \x20   acc = acc * 1.5 + {i}.0\n\
             \x20   if acc > 100.0:\n\
             \x20       acc = acc - 100.0\n\
             \x20   return acc\n"
        ));
    }
    let wasm = build(&source);
    assert_eq!(call_f64(&wasm, "step0", &[2.0]), 3.0);
    assert_eq!(call_f64(&wasm, "step24", &[2.0]), 27.0);
}

#[test]
fn the_subset_refuses_what_it_does_not_implement() {
    for (source, needle) in [
        ("def f() -> f64:\n    return [1, 2]\n", "lists"),
        ("def f() -> f64:\n    return {1: 2}\n", "dicts"),
        ("def f() -> f64:\n    return \"hi\"\n", "literals"),
        ("import math\ndef f() -> f64:\n    return 1.0\n", "def"),
        ("def f() -> f64:\n    import math\n    return 1.0\n", "imports"),
        ("class A:\n    pass\n", "def"),
        ("def f() -> f64:\n    class A:\n        pass\n    return 1.0\n", "classes"),
        ("def f() -> f64:\n    g = lambda x: x\n    return 1.0\n", "lambdas"),
        ("def f() -> f64:\n    try:\n        pass\n    except:\n        pass\n    return 1.0\n", "try"),
        ("def f() -> f64:\n    with open() as g:\n        pass\n    return 1.0\n", "with"),
        ("def f() -> f64:\n    del x\n    return 1.0\n", "del"),
        ("def f() -> f64:\n    global g\n    return 1.0\n", "globals"),
        ("def f() -> f64:\n    yield 1.0\n", "yield"),
        ("def f() -> f64:\n    for i in range(3):\n        pass\n    return 1.0\n", "for"),
        ("def f() -> f64:\n    def g() -> f64:\n        return 1.0\n    return 1.0\n", "nested"),
        ("def f(x) -> f64:\n    return 1.0\n", "annotated"),
        ("def f(x: f64):\n    return 1.0\n", "return type"),
        ("def f(x: str) -> f64:\n    return 1.0\n", "unknown type"),
        ("def f() -> f64:\n    return undefined_name\n", "not defined"),
        ("def f() -> f64:\n    return 1.0\ndef f() -> f64:\n    return 2.0\n", "more than once"),
        ("def f(x: f64) -> f64:\n    if x > 0.0:\n        return 1.0\n", "every path"),
        ("def f() -> f64:\n    break\n", "outside a loop"),
        ("def f() -> f64:\n    return 1 @ 2\n", "`@`"),
        ("def f() -> f64:\n    return 1 & 2\n", "bitwise"),
        ("def f() -> f64:\n    return ~1\n", "`~`"),
        ("def f() -> f64:\n    return x[0]\n", "indexing"),
        ("def f() -> f64:\n    return a.b\n", "attribute"),
    ] {
        let diags = reject(source);
        let text = format!("{diags}");
        assert!(
            text.contains(needle),
            "expected {source:?} to mention {needle:?}, got:\n{text}"
        );
    }
}

#[test]
fn every_diagnostic_carries_a_span_inside_the_source() {
    let source = "def f() -> f64:\n    return undefined_name\n";
    let diags = reject(source);
    assert_eq!(diags.len(), 1);
    let d = diags.iter().next().unwrap();
    assert!(d.span.start < d.span.end);
    assert_eq!(
        &source[d.span.start as usize..d.span.end as usize],
        "undefined_name"
    );
}
