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
pub const COMPILER_VERSION: u32 = 3;

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
    /// Index into `fields` of the sample's own timestamp, an `i64` of
    /// microseconds, when the frame carries one. An input frame does when
    /// its producer stamps its records: the host fills it like any other
    /// field, and `deltat` is what reads it. Its name is not a Python
    /// identifier, so no body can shadow it.
    pub timestamp: Option<usize>,
}

impl Frame {
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The sample-timestamp field, when this frame carries one.
    pub fn timestamp_field(&self) -> Option<&Field> {
        self.timestamp.map(|i| &self.fields[i])
    }
}

/// Where one field of an input frame gets its value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Binding {
    /// A component in the host's tree, by full resolved path.
    Component(String),
    /// A field of another system's output frame, in this same program.
    Produced { system: usize, field: usize },
    /// The output of a [`Stage`] in this same program.
    Resampled { stage: usize },
    /// The producer's own timestamp for the sample the frame is, as `i64`
    /// microseconds — the field [`Frame::timestamp`] names.
    Timestamp,
}

/// How a [`Stage`] fills a tick the input did not land on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resample {
    /// Hold the most recent sample.
    Zoh,
    /// Interpolate between the samples that bracket the tick.
    Linear,
}

/// A clock-changing stage: a top-level binding whose right-hand side is
/// exactly a resample call.
///
/// This is the one construct in the language that is *not* compiled, and the
/// exception is deliberate. Resampling changes which clock a value ticks on,
/// so it is scheduling rather than arithmetic — putting it in a body would put
/// a timer inside the sandbox, which is the one thing the sandbox is for not
/// having. So the checker recognises the shape at the top level, refuses the
/// call anywhere else, and the host wires its own resampler.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stage {
    /// The binding's name, which is also what the stage publishes under.
    pub name: String,
    pub kind: Resample,
    /// What is being resampled: a component, a system's output field, or an
    /// earlier stage.
    pub source: Binding,
    /// Hertz of the clock the output ticks on.
    pub rate: f64,
    /// What the output carries, which is what the input carried.
    pub ty: Ty,
    /// Where this stage's card sits, per its own `# @node` comment.
    pub layout: Layout,
    pub source_span: Span,
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

/// Where a declaration's card sits, according to its own source.
///
/// Layout rides the declaration rather than a sidecar, so the file is
/// self-contained — share the `.py` and the diagram travels with it — and a
/// rename cannot orphan a position, because the position is attached to the
/// declaration rather than keyed by its name. The compiler parses it, carries
/// it, and looks at nothing else about it.
///
/// [`Layout::span`] is what a drag rewrites. An empty span is where a
/// declaration that has never been placed would gain its annotation, so
/// placing one and moving one are the same edit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub position: Option<(f32, f32)>,
    pub span: Span,
    pub form: Form,
}

/// How a position is spelled at one declaration.
///
/// Python has no decorator syntax for an assignment, so a binding or a stage
/// carries the same annotation as a trailing comment. Both are attached to the
/// declaration and both are one region to replace, which is all the canvas
/// needs them to have in common.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Form {
    /// `@node(x=240, y=120)` on its own line above the declaration.
    #[default]
    Decorator,
    /// `# @node(x=240, y=120)` at the end of the declaration's line.
    Comment,
}

impl Layout {
    /// The source with this declaration placed at `(x, y)`.
    ///
    /// The compiler owns the spelling so a host never has to know it; moving a
    /// card and placing one for the first time are the same call, because an
    /// unplaced declaration's span is the empty region its annotation belongs
    /// in. Positions round to whole pixels — sub-pixel placement in a source
    /// file is diff noise, not information.
    pub fn place(&self, source: &str, x: f32, y: f32) -> String {
        let annotation = match self.form {
            Form::Decorator => format!("@node(x={}, y={})\n", x.round(), y.round()),
            Form::Comment => format!("  # @node(x={}, y={})", x.round(), y.round()),
        };
        let mut out = String::with_capacity(source.len() + annotation.len());
        out.push_str(&source[..self.span.start as usize]);
        out.push_str(&annotation);
        out.push_str(&source[self.span.end as usize..]);
        out
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
    /// Where this system's card sits, per its own `@node` decorator.
    pub layout: Layout,
    /// Hertz, when the system clocks itself rather than waiting on an input.
    ///
    /// A source has nothing to fire it, so it says how often it wants to run
    /// and the host supplies the timer. Mutually exclusive with `driving`: a
    /// system is either source-clocked or input-driven, never both.
    pub rate: Option<f64>,
    /// Plain functions called by this system, including indirect calls.
    pub dependencies: Vec<usize>,
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
    /// The function declaration's source region.
    pub source: Span,
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
    /// Host-wired resamplers, in declaration order among themselves.
    pub stages: Vec<Stage>,
    /// Plain `def`s, exported by name and called directly.
    pub functions: Vec<FnSig>,
}

/// One top-level declaration, by which list it lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decl {
    System(usize),
    Stage(usize),
}

impl Manifest {
    pub fn system(&self, name: &str) -> Option<&System> {
        self.systems.iter().find(|s| s.name == name)
    }

    /// Every declaration in the order the source wrote it.
    ///
    /// A host must build in this order, because a binding may name an earlier
    /// declaration and only an earlier one. Order is recovered from the spans
    /// rather than stored: top-level declarations do not overlap, so where
    /// each one starts *is* the order it was written in.
    pub fn declarations(&self) -> Vec<Decl> {
        let mut all: Vec<(u32, Decl)> = self
            .systems
            .iter()
            .enumerate()
            .map(|(i, s)| (s.source.start, Decl::System(i)))
            .chain(
                self.stages
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (s.source_span.start, Decl::Stage(i))),
            )
            .collect();
        all.sort_by_key(|(at, _)| *at);
        all.into_iter().map(|(_, decl)| decl).collect()
    }
}
