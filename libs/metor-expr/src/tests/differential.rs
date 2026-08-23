//! The differential harness: compiled-under-wasmi against native nox.
//!
//! "Python syntax, nox numerics" is only worth saying if something enforces
//! it. This is that thing. A case runs twice — once as a compiled module under
//! fuel-metered `wasmi`, once natively in this process — and the two results
//! must be **bit identical**, not close.
//!
//! ## What plays the oracle, and why it is not always `nox`
//!
//! nox is the reference, but it is a tensor library, not an interpreter, so
//! the reference is drawn from three places and each is deliberate:
//!
//! - **`f64` arithmetic and the ops nox has** — `+ - * /`, `sqrt`, `sin`,
//!   `cos`, `asin`, `acos`, `atan2` — go through `Scalar<f64, ArrayRepr>`,
//!   nox's rank-0 tensor. This is the fixed-shape path, which the M0 spike
//!   established is the correct one; the `Dyn` path is not used here or
//!   anywhere else in this crate.
//! - **`i64` arithmetic** is Rust's `i64`, because that *is* nox's semantics:
//!   an integer tensor's element type is `i64` and its arithmetic is the
//!   element type's. Wrapping is therefore the reference behaviour, not a
//!   compromise. Python's floor rules for `//` and `%` are spelled out here
//!   rather than borrowed, since Rust's `/` and `%` truncate.
//! - **`exp`, `log`, `tan`, `pow`, and the hyperbolics** come from `libm`
//!   **directly**, because they are not in nox's `RealField` at all. This
//!   matters more than it looks: the M0 spike measured guest `libm::exp(1.0)`
//!   and host `f64::exp(1.0)` differing by one ULP. Calling `std` here would
//!   make the harness fail on correct code. nox built with
//!   `default-features = false` — which is how this workspace pins it —
//!   routes its own `RealField` through `libm` for the same reason, so this
//!   keeps one implementation across guest, oracle, and nox.

use nox::{Array, ArrayRepr, ReprMonad, Scalar, Tensor};

use super::{build, call_f64};

/// A rank-0 nox tensor — the oracle's scalar.
pub(super) fn nox_scalar(v: f64) -> Scalar<f64, ArrayRepr> {
    Tensor::from_inner(Array::from(v))
}

/// Read a nox scalar back out.
pub(super) fn nox_value(t: Scalar<f64, ArrayRepr>) -> f64 {
    t.into_inner().view().buf()[0]
}

/// Compile an `f64` expression, run it under wasmi, and require it to equal
/// what nox computes natively.
pub(super) fn agrees(expr: &str, oracle: impl FnOnce() -> f64) {
    let source = format!("def f() -> f64:\n    return {expr}\n");
    let compiled = call_f64(&build(&source), "f", &[]);
    let native = oracle();
    assert_eq!(
        compiled.to_bits(),
        native.to_bits(),
        "{expr}: compiled {compiled} but nox says {native}"
    );
}

/// The same, for a function of `f64` arguments evaluated at several points.
pub(super) fn agrees_over(
    source: &str,
    name: &str,
    points: &[&[f64]],
    oracle: impl Fn(&[f64]) -> f64,
) {
    let wasm = build(source);
    for args in points {
        let compiled = call_f64(&wasm, name, args);
        let native = oracle(args);
        assert_eq!(
            compiled.to_bits(),
            native.to_bits(),
            "{name}{args:?}: compiled {compiled} but nox says {native}"
        );
    }
}

/// Python's `//` on integers, which floors where Rust truncates.
pub(super) fn py_floordiv(a: i64, b: i64) -> i64 {
    let q = a.wrapping_div(b);
    let r = a.wrapping_rem(b);
    if r != 0 && (r < 0) != (b < 0) { q - 1 } else { q }
}

/// Python's `%` on integers, whose sign follows the divisor.
pub(super) fn py_rem(a: i64, b: i64) -> i64 {
    let r = a.wrapping_rem(b);
    if r != 0 && (r < 0) != (b < 0) { r + b } else { r }
}

#[test]
fn the_oracle_round_trips() {
    assert_eq!(nox_value(nox_scalar(1.5)), 1.5);
    let sum = nox_scalar(1.5) + nox_scalar(2.25);
    assert_eq!(nox_value(sum), 3.75);
}

/// The harness is only as good as its willingness to fail.
#[test]
#[should_panic(expected = "but nox says")]
fn a_disagreement_is_a_failure() {
    agrees("1.0 + 1.0", || nox_value(nox_scalar(1.0) + nox_scalar(2.0)));
}
