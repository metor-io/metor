//! A typed subset of Python, compiled to closed WebAssembly modules.
//!
//! The premise, from `metor-fsw-2/docs/plans/python-expressions.md`: the panel
//! and the vehicle should run *the same artifact*, an operator should write it
//! in the language they already write `target.py` in, and typing a character
//! into an expression field should not be able to stall anything. That rules
//! out shipping an interpreter as the guest (no static output schema, no
//! closed module, fuel that meters dispatch rather than user work) and it
//! rules out any pipeline with a toolchain in it. What is left is a compiler
//! small enough to live in the flight binary: parse, check, encode.
//!
//! ```
//! let module = metor_expr::compile("def f(x: f64) -> f64:\n    return x * 2.0\n").unwrap();
//! assert_eq!(module.manifest.functions[0].name, "f");
//! ```
//!
//! ## A frame is a class, a system is a decorated function
//!
//! The unit of the language is the one metor-fsw-2 already has. A `Frame`
//! class is a named record of values sharing a timestamp; a `@system` is a
//! function whose signature *is* its wiring surface — frame parameters are
//! input ports, a `State` parameter is what it keeps, the return frame is what
//! it publishes. There is no `persist`, because publishing is what returning
//! means, and no `source`, because binding is what the signature means.
//!
//! Three tiers write the same construct, which is what makes promoting one to
//! the next a copy-paste: `@system` over declared frames, `@system` over
//! positional component paths, and a bare expression — `adcs.omega_b * 100` is
//! already valid Python, so a one-liner in a panel field is a system that
//! never needed a name. `lang` reduces all three to one shape
//! before anything is checked.
//!
//! ## Names resolve here and stay resolved
//!
//! The compiler cannot know what `adcs.omega_b` is, so it asks a
//! [`Resolver`] — the panel over its db vtables, the vehicle over its frame
//! registry — and asks *once*. A bare name resolving by unique suffix is a
//! fact about the component tree at authoring time; what the [`Manifest`]
//! records is the full path it found, so a component added tomorrow cannot
//! change what a saved expression reads.
//!
//! ## Nothing links at compile time
//!
//! `wasm32-unknown-unknown` is needed to *regenerate* the prelude, never to
//! compile an expression. The prelude — `prelude/`, built by
//! `scripts/regen-prelude.sh` — is checked in as [`PRELUDE`], and compiling
//! means appending functions to those bytes: new types, new bodies, new
//! exports, new data, same memory. [`template`] carries that, including the
//! call-graph walk that drops kernels a program never reaches, so a module
//! that wanted `sin` does not also ship a matmul.
//!
//! Scalar arithmetic never enters the prelude at all. `a * b + c` is native
//! wasm opcodes; the prelude is entered for transcendentals, for tensor
//! kernels, and for nothing else.
//!
//! ## Tensors are addresses, and so are frames
//!
//! Every tensor in a compiled program lives at an address the compiler picked,
//! above the linker's `__heap_base`. That is what makes kernel call sites
//! constant, and it is why the guest needs no allocator. It also gives the
//! host ABI its shape: a function whose signature mentions a tensor takes no
//! wasm parameters, and is driven through `<name>_arg_ptr(i)`, `<name>()`,
//! `<name>_ret_ptr()`. See [`FnSig::uses_buffers`].
//!
//! A frame is the same idea one level up: one block per port, addressed by
//! `<system>_arg_ptr(i)`, with each field a constant offset the [`Manifest`]
//! spells out. A system is therefore driven as
//! `<system>_arg_ptr(i)` / `<system>_state_ptr(i)` / `<system>_eval(now)` /
//! `<system>_ret_ptr()`, and nothing about that is discovered by convention.
//!
//! ## The pipeline
//!
//! [`compile_module`] is four passes and no more. `rustpython-parser` produces
//! a Python AST; `lang` reads its declarations and resolves every
//! binding; `check` turns bodies into a typed IR, which is the
//! only representation of a program this crate keeps; `codegen`
//! emits. The checker is the *single* validation gate — codegen re-derives
//! nothing and guards against nothing, because everything it could guard
//! against was already refused with a span.
//!
//! ## Numerics are nox's, and nox is the oracle
//!
//! The subset's semantics are a tensor library's, not CPython's: ints are
//! `i64` and wrap, `/` is always true division, `//` and `%` floor. Every
//! semantic test therefore runs twice — compiled, under fuel-metered wasmi,
//! and natively against nox in-process — and the results must agree. There is
//! no second interpreter in the loop. See `tests::differential` for the
//! harness and for the one place the oracle needed care.
//!
//! The M0 spike qualified nox's role in one place, by measurement. nox's
//! *fixed shape* path (`Tensor<f64, Const<N>, ArrayRepr>`) is correct and well
//! exercised, and it is the oracle. Its *dynamic* shape path is not: an
//! `Array<T, Dyn>` is `Vec`-backed so every operation allocates,
//! `RealField: faer::ComplexField` pulls faer's gemm into the guest, and
//! `DynArray::default` sizes buffers by the sum of the dims instead of their
//! product, so a `Dyn` 3×3 gets six elements. The guest therefore takes
//! `nox_array::ArrayView` — the shape-plus-slice view, which has no
//! dependencies beyond `zerocopy` — and writes its loops out by hand. See the
//! plan's Results section for the numbers behind that.

mod check;
mod codegen;
mod diag;
mod ir;
mod lang;
mod manifest;
mod pack;
mod pack_abi;
mod resolve;
pub mod state;
pub mod template;

pub use diag::{Diagnostic, Diagnostics, Span};
pub use manifest::{
    Binding, COMPILER_VERSION, Decl, Field, FnSig, Form, Frame, Init, Layout, Manifest, Port,
    Resample, Stage, StateField, System,
};
pub use pack::{ComponentSource, PackProgram, PackResolver, compile_pack};
pub use resolve::{CompSchema, FrameSchema, Resolver, Unresolved};

/// Decode the manifest a module carries behind `expr_manifest_ptr`.
///
/// A compiled module says what it is without the compiler that made it, which
/// is what lets one be saved, uplinked, or handed to a different host.
pub fn describe(bytes: &[u8]) -> Result<Manifest, postcard::Error> {
    postcard::from_bytes(bytes)
}

/// The guest template generated functions are appended to.
///
/// Regenerated by `scripts/regen-prelude.sh`, which is the only thing in this
/// crate that needs a wasm toolchain.
pub const PRELUDE: &[u8] = include_bytes!("prelude.wasm");

/// An element type. Only `f64` participates in tensors today; the rest of the
/// vocabulary is here because the manifest is what hosts serialize.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Dtype {
    F64,
    I64,
    Bool,
}

/// A value's type in the subset.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Ty {
    F64,
    I64,
    Bool,
    Tensor { dtype: Dtype, shape: Vec<usize> },
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::F64 => write!(f, "f64"),
            Ty::I64 => write!(f, "i64"),
            Ty::Bool => write!(f, "bool"),
            Ty::Tensor { dtype, shape } => {
                let dtype = match dtype {
                    Dtype::F64 => "f64",
                    Dtype::I64 => "i64",
                    Dtype::Bool => "bool",
                };
                match shape.as_slice() {
                    [n] => write!(f, "Tensor[{dtype}, {n}]"),
                    dims => {
                        write!(f, "Tensor[{dtype}, (")?;
                        for (i, d) in dims.iter().enumerate() {
                            if i > 0 {
                                write!(f, ", ")?;
                            }
                            write!(f, "{d}")?;
                        }
                        write!(f, ")]")
                    }
                }
            }
        }
    }
}

/// A compiled program: a closed module, and the manifest to drive it by.
#[derive(Clone, Debug)]
pub struct Program {
    /// A complete core module. No imports, ever.
    pub wasm: Vec<u8>,
    pub manifest: Manifest,
}

/// Compile a module of annotated `def`s, against a host that knows nothing.
///
/// Never panics. Any input that is not a well-typed program in the subset —
/// including a half-typed line, a truncated file, or arbitrary bytes — comes
/// back as [`Diagnostics`] carrying spans.
pub fn compile(source: &str) -> Result<Program, Diagnostics> {
    compile_module(source, &Unresolved)
}

/// Compile a module of frames, systems, bindings, and helper `def`s.
pub fn compile_module(source: &str, resolver: &dyn Resolver) -> Result<Program, Diagnostics> {
    emit(check::check(source, resolver, check::Entry::Module)?)
}

/// Compile one bare expression into the single anonymous system a `=` field
/// runs, exported as `expr_eval`.
pub fn compile_expr(source: &str, resolver: &dyn Resolver) -> Result<Program, Diagnostics> {
    emit(check::check(source, resolver, check::Entry::Expression)?)
}

fn emit((program, manifest): (ir::Program, Manifest)) -> Result<Program, Diagnostics> {
    Ok(Program {
        wasm: codegen::emit(&program, &manifest),
        manifest,
    })
}

#[cfg(test)]
mod tests;
