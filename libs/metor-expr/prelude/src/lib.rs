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
//! - **No imports, no allocator.** The module must stay closed, so the panic
//!   handler traps rather than reporting, and every kernel writes through
//!   caller-supplied pointers into linear memory. Shapes arrive as arguments
//!   because the compiler knows them statically; the guest never sizes
//!   anything at run time.
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
//! ## The arena
//!
//! [`expr_arena`] hands back the base of a fixed `static mut` block. Argument
//! and return buffers for tensor-typed functions are carved out of it by the
//! compiler at fixed offsets, which is what lets a generated `expr_arg_ptr`
//! be a constant plus a call. Reserving it in the guest source rather than
//! appending data segments keeps the linker's memory layout authoritative —
//! nothing the compiler adds can land on the shadow stack.

#![no_std]

/// Bytes of linear memory the compiler may carve argument, return, and
/// spill buffers out of.
pub const ARENA_LEN: usize = 16 * 1024;

static mut ARENA: [u8; ARENA_LEN] = [0; ARENA_LEN];

/// Base address of the compiler-owned arena in linear memory.
#[unsafe(no_mangle)]
pub extern "C" fn expr_arena() -> u32 {
    &raw const ARENA as u32
}

/// Bytes available at [`expr_arena`].
#[unsafe(no_mangle)]
pub extern "C" fn expr_arena_len() -> u32 {
    ARENA_LEN as u32
}

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

unary!(sin, cos, tan, asin, acos, atan, exp, log, sinh, cosh, tanh, floor, ceil, round, trunc);

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
    if r != 0.0 && (r < 0.0) != (y < 0.0) { r + y } else { r }
}

#[cfg(feature = "tensor-kernels")]
mod tensor;

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
