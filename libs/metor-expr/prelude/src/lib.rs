//! The template module every compiled expression is emitted into.
//!
//! `metor-expr` does not link a guest at expression-compile time — there is no
//! toolchain in a panel keystroke. Instead this crate is compiled to
//! `wasm32-unknown-unknown` once, checked in as `prelude.wasm`, and the
//! compiler *appends* generated functions to that module: new types, new
//! bodies, new exports, same memory and same data. A generated function that
//! needs `sin` or an elementwise vector add calls a kernel that is already
//! there, at an index the compiler reads out of the template's export table.
//!
//! Two rules shape everything here.
//!
//! - **No imports, and no allocation on the evaluation path.** The module
//!   must stay closed, so a panic traps rather than reporting (wasm32 with
//!   `panic = "abort"` aborts; nothing unwinds), and every kernel writes
//!   through caller-supplied pointers into linear memory. Shapes arrive as
//!   arguments because the compiler knows them statically. The [`pack`]
//!   module's ring handles do allocate — once, at bind time, before the host
//!   pins guest memory — and nothing allocates per evaluation.
//! - **The tensor layer is `nox_array`, not `nox`.** The M0 spike measured
//!   nox's `Dyn` path and rejected it: `Array<T, Dyn>` is `Vec`-backed, so
//!   every operation would allocate; `RealField: faer::ComplexField` drags
//!   faer's gemm into the guest; and the path is numerically wrong today
//!   (`DynArray::default` sizes buffers by the *sum* of the dims rather than
//!   their product). What survives is `nox_array::ArrayView` — a shape plus a
//!   slice, zero dependencies — with the loops written out here. Native nox
//!   at fixed shapes stays the harness's oracle, which is where it is correct
//!   and well exercised.
//!
//! Transcendentals come from `libm` directly. Scalar arithmetic never enters
//! this module at all: the compiler emits native wasm opcodes for it, and only
//! reaches in here for the functions wasm has no instruction for.
//!
//! ## Where the compiler's buffers go
//!
//! This crate reserves nothing for them. Everything above the linker's
//! `__heap_base` is unclaimed at link time, so the compiler lays argument,
//! return, and temporary buffers out from that address, appends data segments
//! for the ones with constant contents, and raises the memory minimum to
//! cover the rest. The allocator (dlmalloc, serving [`pack`]'s bind-time
//! handles and `fsw_pack_alloc`) grows fresh pages via `memory.grow`, so it
//! can never hand out bytes the compiler placed below the raised minimum.

macro_rules! unary {
    ($($name:ident),* $(,)?) => {
        $(
            #[unsafe(no_mangle)]
            pub extern "C" fn $name(x: f64) -> f64 {
                libm::$name(x)
            }
        )*
    };
}

unary!(
    sin, cos, tan, asin, acos, atan, exp, log, sinh, cosh, tanh, floor, ceil, round, trunc
);

/// Two-argument arctangent, quadrant-aware.
#[unsafe(no_mangle)]
pub extern "C" fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

/// `x` raised to `y`, the fallback for `**` with a non-literal exponent.
#[unsafe(no_mangle)]
pub extern "C" fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

/// Floored remainder — Python's `%`, not wasm's truncating `rem`.
#[unsafe(no_mangle)]
pub extern "C" fn fmod_floor(x: f64, y: f64) -> f64 {
    let r = libm::fmod(x, y);
    if r != 0.0 && (r < 0.0) != (y < 0.0) {
        r + y
    } else {
        r
    }
}

/// Advance a splitmix64 state word in place and return a uniform `f64` in
/// `[0, 1)`.
///
/// The generator lives in the guest so that `random()` costs one call and no
/// import, and its state lives in a state slot so that it survives an edit the
/// way a filter's memory does. The host writes that slot at instantiation —
/// zero is a legal splitmix64 seed but a shared one, and two systems drawing
/// the same sequence would be a surprise.
#[unsafe(no_mangle)]
pub extern "C" fn rng_unit(state: *mut u64) -> f64 {
    let s = unsafe { &mut *state };
    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(feature = "tensor-kernels")]
mod tensor;

mod pack;
