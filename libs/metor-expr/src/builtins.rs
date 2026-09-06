//! The built-in functions, as a table.
//!
//! The checker knows these by matching on names ([`check`](crate::check)'s
//! `builtin` and `transcendental`), which is right for compiling and useless
//! for offering: a completion needs the list up front, with signatures and a
//! word of documentation. This table is that list. It describes what the
//! checker accepts — the drift test in this module holds the two together.

/// Where a call to a builtin may appear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Avail {
    /// Any expression.
    Anywhere,
    /// Only inside a system's body, where there is a clock and a state
    /// record to hang it on.
    System,
    /// Only as a top-level binding's whole right-hand side — resampling
    /// changes the clock, which is the host's job, not arithmetic.
    TopLevel,
}

/// One built-in function: what to call it, what it takes, where it works.
#[derive(Clone, Copy, Debug)]
pub struct Builtin {
    pub name: &'static str,
    pub params: &'static [&'static str],
    /// The result type as the language spells it. Descriptive only — `abs`
    /// and `constant` follow their argument.
    pub ret: &'static str,
    pub avail: Avail,
    pub doc: &'static str,
}

/// Every function the language defines, in offer order: the everyday
/// arithmetic first, the system-only generators last.
pub fn builtins() -> &'static [Builtin] {
    use Avail::*;
    const B: &[Builtin] = &[
        Builtin {
            name: "abs",
            params: &["x"],
            ret: "number",
            avail: Anywhere,
            doc: "absolute value, keeping the argument's type",
        },
        Builtin {
            name: "min",
            params: &["a", "b"],
            ret: "number",
            avail: Anywhere,
            doc: "the smaller of two numbers",
        },
        Builtin {
            name: "max",
            params: &["a", "b"],
            ret: "number",
            avail: Anywhere,
            doc: "the larger of two numbers",
        },
        Builtin {
            name: "sqrt",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "square root",
        },
        Builtin {
            name: "floor",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "round toward negative infinity",
        },
        Builtin {
            name: "ceil",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "round toward positive infinity",
        },
        Builtin {
            name: "trunc",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "round toward zero",
        },
        Builtin {
            name: "round",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "round to the nearest integer",
        },
        Builtin {
            name: "clamp",
            params: &["x", "lo", "hi"],
            ret: "number",
            avail: Anywhere,
            doc: "x limited to [lo, hi]",
        },
        Builtin {
            name: "sign",
            params: &["x"],
            ret: "number",
            avail: Anywhere,
            doc: "-1, 0, or 1 by the argument's sign",
        },
        Builtin {
            name: "int",
            params: &["x"],
            ret: "i64",
            avail: Anywhere,
            doc: "truncate to an integer",
        },
        Builtin {
            name: "float",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "widen to a float",
        },
        Builtin {
            name: "pow",
            params: &["x", "y"],
            ret: "f64",
            avail: Anywhere,
            doc: "x raised to y",
        },
        Builtin {
            name: "hypot",
            params: &["x", "y"],
            ret: "f64",
            avail: Anywhere,
            doc: "the length of (x, y)",
        },
        Builtin {
            name: "lerp",
            params: &["a", "b", "t"],
            ret: "f64",
            avail: Anywhere,
            doc: "a at t = 0, b at t = 1, linear between",
        },
        Builtin {
            name: "atan2",
            params: &["y", "x"],
            ret: "f64",
            avail: Anywhere,
            doc: "the angle of (x, y), quadrant-correct",
        },
        Builtin {
            name: "degrees",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "radians to degrees",
        },
        Builtin {
            name: "radians",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "degrees to radians",
        },
        Builtin {
            name: "sin",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "sine, radians",
        },
        Builtin {
            name: "cos",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "cosine, radians",
        },
        Builtin {
            name: "tan",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "tangent, radians",
        },
        Builtin {
            name: "asin",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "arcsine",
        },
        Builtin {
            name: "acos",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "arccosine",
        },
        Builtin {
            name: "atan",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "arctangent",
        },
        Builtin {
            name: "exp",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "e raised to x",
        },
        Builtin {
            name: "log",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "natural logarithm",
        },
        Builtin {
            name: "log2",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "base-2 logarithm",
        },
        Builtin {
            name: "log10",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "base-10 logarithm",
        },
        Builtin {
            name: "sinh",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "hyperbolic sine",
        },
        Builtin {
            name: "cosh",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "hyperbolic cosine",
        },
        Builtin {
            name: "tanh",
            params: &["x"],
            ret: "f64",
            avail: Anywhere,
            doc: "hyperbolic tangent",
        },
        Builtin {
            name: "sum",
            params: &["t"],
            ret: "f64",
            avail: Anywhere,
            doc: "the sum of a tensor's elements",
        },
        Builtin {
            name: "mean",
            params: &["t"],
            ret: "f64",
            avail: Anywhere,
            doc: "the average of a tensor's elements",
        },
        Builtin {
            name: "mean",
            params: &["x", "n"],
            ret: "f64",
            avail: System,
            doc: "the average of the last n samples of x",
        },
        Builtin {
            name: "len",
            params: &["t"],
            ret: "i64",
            avail: Anywhere,
            doc: "a tensor's leading extent",
        },
        Builtin {
            name: "fft",
            params: &["t"],
            ret: "Tensor",
            avail: Anywhere,
            doc: "power spectrum over the last axis",
        },
        Builtin {
            name: "constant",
            params: &["v"],
            ret: "number",
            avail: Anywhere,
            doc: "its argument, unchanged",
        },
        Builtin {
            name: "now",
            params: &[],
            ret: "i64",
            avail: System,
            doc: "the current tick's timestamp",
        },
        Builtin {
            name: "random",
            params: &[],
            ret: "f64",
            avail: System,
            doc: "uniform in [0, 1), one sequence per system",
        },
        Builtin {
            name: "sine",
            params: &["freq", "amp"],
            ret: "f64",
            avail: System,
            doc: "sine wave of the system's clock",
        },
        Builtin {
            name: "cosine",
            params: &["freq", "amp"],
            ret: "f64",
            avail: System,
            doc: "cosine wave of the system's clock",
        },
        Builtin {
            name: "square",
            params: &["freq", "amp"],
            ret: "f64",
            avail: System,
            doc: "square wave of the system's clock",
        },
        Builtin {
            name: "sawtooth",
            params: &["freq", "amp"],
            ret: "f64",
            avail: System,
            doc: "sawtooth wave of the system's clock",
        },
        Builtin {
            name: "window",
            params: &["x", "n"],
            ret: "Tensor",
            avail: System,
            doc: "the last n samples of x, oldest first",
        },
        Builtin {
            name: "delta",
            params: &["x"],
            ret: "f64",
            avail: System,
            doc: "x minus its previous sample, 0 on the first",
        },
        Builtin {
            name: "deltat",
            params: &["x"],
            ret: "f64",
            avail: System,
            doc: "seconds between the input x's newest sample and the one before",
        },
        Builtin {
            name: "resample_zoh",
            params: &["source", "rate"],
            ret: "channel",
            avail: TopLevel,
            doc: "hold the last sample onto a new clock",
        },
        Builtin {
            name: "resample_linear",
            params: &["source", "rate"],
            ret: "channel",
            avail: TopLevel,
            doc: "interpolate onto a new clock",
        },
    ];
    B
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table entry names something the checker recognises: probing it
    /// must never produce the checker's "is not defined" fallback. The other
    /// direction — a builtin added to the checker but not here — has no
    /// automated guard; add the row when adding the match arm.
    #[test]
    fn table_matches_checker() {
        for b in builtins() {
            let args = b
                .params
                .iter()
                .map(|_| "0.0")
                .collect::<Vec<_>>()
                .join(", ");
            let probe = format!("out = {}({args})", b.name);
            let complaint = format!("`{}` is not defined", b.name);
            match crate::compile(&probe) {
                Ok(_) => {}
                Err(diags) => {
                    assert!(
                        diags.iter().all(|d| d.message != complaint),
                        "`{}` is in the table but the checker does not know it",
                        b.name
                    );
                }
            }
        }
    }
}
