//! What a compiled program tells its host.
//!
//! The manifest *is* the signature. A host reads it and knows every address it
//! must write, every component it must subscribe to, which of them makes the
//! system fire, what the outputs are called, and what state has to survive an
//! edit. Nothing about driving a module is discovered by convention — the
//! layout of every frame buffer is here, field by field, in the order the
//! fields occupy memory.
//!
//! Two properties are worth naming, because later phases lean on them.
//!
//! **Bindings are resolved.** A [`Port`] carries full component paths, not the
//! text the operator typed. The suffix search that produced them ran once, at
//! authoring time; a saved program is read back through the resolved path and
//! is immune to a component added later that would have made the name
//! ambiguous.
//!
//! **Systems are individually hashed.** Each [`System`] carries the source
//! span it came from, so an edit to one body rebuilds one instance and leaves
//! the rest — and their state — running.

use crate::{Span, Ty};

use serde::{Deserialize, Serialize};

/// The compiler revision a manifest was produced by. Hosts refuse a manifest
/// they do not recognise rather than guessing at a layout.
pub const COMPILER_VERSION: u32 = 1;

/// One field of a frame or state record.
///
/// Every element occupies eight bytes, so a field is always `f64`-aligned and
/// a host never has to reason about packing. A `bool` uses the low four of its
/// eight and leaves the rest alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub ty: Ty,
    /// Byte offset within the record's buffer.
    pub offset: u32,
}

/// A named record of values sharing one timestamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub name: String,
    pub fields: Vec<Field>,
    /// Total bytes, which is what `<system>_arg_ptr(i)` addresses.
    pub bytes: u32,
}

impl Frame {
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// Where one field of an input frame gets its value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Binding {
    /// A component in the host's tree, by full resolved path.
    Component(String),
    /// A field of another system's output frame, in this same program.
    Produced { system: usize, field: usize },
}

/// One input frame of a system, and what fills it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    /// The parameter this frame arrives as.
    pub param: String,
    pub frame: Frame,
    /// One entry per field of `frame`, in the same order.
    pub bindings: Vec<Binding>,
}

/// A state field: a value that outlives one evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateField {
    pub name: String,
    pub ty: Ty,
    pub default: Init,
}

/// A state field's initial value, from its annotation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Init {
    F64(f64),
    I64(i64),
    Bool(bool),
    /// Every element of a tensor set to the same scalar.
    Fill(f64),
}

impl Init {
    /// Whether a fresh linear memory already holds this value, in which case
    /// the seed needs no instructions.
    pub fn is_zero(&self) -> bool {
        match self {
            Init::F64(v) | Init::Fill(v) => v.to_bits() == 0,
            Init::I64(v) => *v == 0,
            Init::Bool(v) => !*v,
        }
    }
}

/// One `@system`: its ports, its output, its state, and what makes it fire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct System {
    pub name: String,
    pub inputs: Vec<Port>,
    pub output: Frame,
    /// The component id each output field publishes under, in field order.
    pub publishes: Vec<String>,
    pub state: Vec<StateField>,
    /// Index into `inputs` of the port whose publication drives the system.
    /// Absent when the system is source-clocked, and when it has no inputs at
    /// all.
    pub driving: Option<usize>,
    /// Hertz, when the system clocks itself rather than waiting on an input.
    ///
    /// A source has nothing to fire it, so it says how often it wants to run
    /// and the host supplies the timer. Mutually exclusive with `driving`: a
    /// system is either source-clocked or input-driven, never both.
    pub rate: Option<f64>,
    /// The source region this system was written in — the input to a
    /// per-system content hash, so an edit rebuilds only what changed.
    pub source: Span,
}

/// One exported plain function: what to call it and what it takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FnSig {
    pub name: String,
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
}

impl FnSig {
    /// Whether this function crosses through static buffers rather than by
    /// value.
    ///
    /// A tensor anywhere in the signature moves the whole signature into
    /// memory: the host writes argument `i` at `<name>_arg_ptr(i)`, calls
    /// `<name>()`, and reads the result at `<name>_ret_ptr()`. Scalar-only
    /// functions keep taking and returning wasm values.
    pub fn uses_buffers(&self) -> bool {
        matches!(self.ret, Ty::Tensor { .. })
            || self
                .params
                .iter()
                .any(|(_, ty)| matches!(ty, Ty::Tensor { .. }))
    }
}

/// What the host needs in order to drive a compiled module.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub compiler: u32,
    pub systems: Vec<System>,
    /// Plain `def`s, exported by name and called directly.
    pub functions: Vec<FnSig>,
}

impl Manifest {
    pub fn system(&self, name: &str) -> Option<&System> {
        self.systems.iter().find(|s| s.name == name)
    }
}
