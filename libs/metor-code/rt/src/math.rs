//! The transcendental functions compiled code calls, from the `libm` crate
//! rather than the platform's, so every target gives the same bits.

/// One math function compiled code can call.
#[derive(Clone, Copy, Debug)]
pub struct MathFn {
    /// The name in metor-code, such as `atan2`.
    pub name: &'static str,
    /// The symbol compiled code calls.
    pub symbol: &'static str,
    pub arity: usize,
    pub f32: bool,
    /// The function's address, for a JIT to bind the symbol to.
    pub addr: *const u8,
}

// SAFETY: `addr` is the address of a function, which never changes.
unsafe impl Sync for MathFn {}
// SAFETY: as above.
unsafe impl Send for MathFn {}

macro_rules! math {
    ($( $name:literal: $f64:ident = $l64:ident, $f32:ident = $l32:ident, $arity:tt; )*) => {
        $(
            math!(@fn $f64, $l64, f64, $arity);
            math!(@fn $f32, $l32, f32, $arity);
        )*

        /// Every math function, in `f64` and `f32` forms.
        pub static MATH: &[MathFn] = &[
            $(
                MathFn { name: $name, symbol: stringify!($f64), arity: $arity, f32: false, addr: $f64 as *const u8 },
                MathFn { name: $name, symbol: stringify!($f32), arity: $arity, f32: true, addr: $f32 as *const u8 },
            )*
        ];
    };
    (@fn $sym:ident, $libm:ident, $t:ty, 1) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $sym(x: $t) -> $t {
            libm::$libm(x)
        }
    };
    (@fn $sym:ident, $libm:ident, $t:ty, 2) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $sym(x: $t, y: $t) -> $t {
            libm::$libm(x, y)
        }
    };
}

math! {
    "sin": mc_rt_sin = sin, mc_rt_sinf = sinf, 1;
    "cos": mc_rt_cos = cos, mc_rt_cosf = cosf, 1;
    "tan": mc_rt_tan = tan, mc_rt_tanf = tanf, 1;
    "asin": mc_rt_asin = asin, mc_rt_asinf = asinf, 1;
    "acos": mc_rt_acos = acos, mc_rt_acosf = acosf, 1;
    "atan": mc_rt_atan = atan, mc_rt_atanf = atanf, 1;
    "atan2": mc_rt_atan2 = atan2, mc_rt_atan2f = atan2f, 2;
    "sinh": mc_rt_sinh = sinh, mc_rt_sinhf = sinhf, 1;
    "cosh": mc_rt_cosh = cosh, mc_rt_coshf = coshf, 1;
    "tanh": mc_rt_tanh = tanh, mc_rt_tanhf = tanhf, 1;
    "exp": mc_rt_exp = exp, mc_rt_expf = expf, 1;
    "ln": mc_rt_ln = log, mc_rt_lnf = logf, 1;
    "log10": mc_rt_log10 = log10, mc_rt_log10f = log10f, 1;
    "log2": mc_rt_log2 = log2, mc_rt_log2f = log2f, 1;
    "pow": mc_rt_pow = pow, mc_rt_powf = powf, 2;
    "hypot": mc_rt_hypot = hypot, mc_rt_hypotf = hypotf, 2;
    "fmod": mc_rt_fmod = fmod, mc_rt_fmodf = fmodf, 2;
}

macro_rules! rounding {
    ($( $sym:ident = $libm:ident: $t:ty, $f32:literal; )*) => {
        $(
            #[unsafe(no_mangle)]
            pub extern "C" fn $sym(x: $t) -> $t {
                libm::$libm(x)
            }
        )*

        /// The rounding functions Cranelift calls on a target without the
        /// instruction.
        pub static ROUNDING: &[MathFn] = &[$(MathFn {
            name: stringify!($sym),
            symbol: stringify!($sym),
            arity: 1,
            f32: $f32,
            addr: $sym as *const u8,
        },)*];
    };
}

rounding! {
    mc_rt_floorf = floorf: f32, true;
    mc_rt_floor = floor: f64, false;
    mc_rt_ceilf = ceilf: f32, true;
    mc_rt_ceil = ceil: f64, false;
    mc_rt_truncf = truncf: f32, true;
    mc_rt_trunc = trunc: f64, false;
    mc_rt_nearestf = rintf: f32, true;
    mc_rt_nearest = rint: f64, false;
}

/// Returns the math function `name` in the given width.
pub fn find(name: &str, f32: bool) -> Option<&'static MathFn> {
    MATH.iter().find(|m| m.name == name && m.f32 == f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_math_table() {
        let atan2 = find("atan2", false).expect("atan2");
        assert_eq!((atan2.arity, atan2.symbol), (2, "mc_rt_atan2"));
        assert_eq!(find("sin", true).map(|m| m.symbol), Some("mc_rt_sinf"));
        assert!(find("gamma", false).is_none());
        assert_eq!(mc_rt_atan2(1.0, 1.0), core::f64::consts::FRAC_PI_4);
    }
}
