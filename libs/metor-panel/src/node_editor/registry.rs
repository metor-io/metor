//! Per-op metadata used by the palette, the canvas renderer, and the
//! connection validator. Single source of truth for socket counts and types
//! so the three subsystems can't drift.

use metor_proto::types::PrimType;
use smallvec::SmallVec;

use crate::dynamic::ops::compose::BinaryOp;
use crate::dynamic::ops::derive::{AffineOp, ThresholdOp, UnaryOp};
use crate::dynamic::ops::generators::Waveform;
use crate::dynamic::ops::resample::ResampleMode;
use crate::dynamic::tensor::TypedScalar;
use crate::node_editor::spec::{NodeSpec, NodeSpecKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketKind {
    /// `ValueType::Clock`.
    Clock,
    /// `ValueType::Value(F64 scalar)`.
    F64Scalar,
    /// Any non-clock value — scalar or vector. Used by ops that record or
    /// pass values through without inspecting the schema (e.g. `Persist`).
    Value,
    /// Any value (clock or value) — used for `ClockOf`'s input, which accepts
    /// either since it just taps the timestamp stream.
    Any,
}

impl SocketKind {
    /// Can data flowing out of `source` be plugged into a socket of kind `target`?
    pub fn compatible_with(self, target: SocketKind) -> bool {
        use SocketKind::*;
        matches!(
            (self, target),
            (Any, _)
                | (_, Any)
                | (Clock, Clock)
                | (F64Scalar, F64Scalar)
                | (F64Scalar, Value)
                | (Value, Value)
        )
    }
}

/// Variadic-or-fixed input arity. Mean is the only variadic op today.
#[derive(Clone, Copy, Debug)]
pub enum Arity {
    Exact(&'static [SocketKind]),
    Variadic { kind: SocketKind, min: usize },
}

impl Arity {
    pub fn socket_at(&self, index: usize) -> Option<SocketKind> {
        match self {
            Arity::Exact(slots) => slots.get(index).copied(),
            Arity::Variadic { kind, .. } => Some(*kind),
        }
    }

    pub fn min_inputs(&self) -> usize {
        match self {
            Arity::Exact(slots) => slots.len(),
            Arity::Variadic { min, .. } => *min,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OpDescriptor {
    pub kind: NodeSpecKind,
    pub label: &'static str,
    pub category: &'static str,
    pub inputs: Arity,
    pub output: SocketKind,
    pub default_spec: fn() -> NodeSpec,
    /// Number of inline arg rows the inspector renders inside the node card.
    /// Must match what `inspector_rows::rows_for_node` returns for this op.
    pub arg_count: usize,
}

const F64: SocketKind = SocketKind::F64Scalar;
const CLK: SocketKind = SocketKind::Clock;
const VAL: SocketKind = SocketKind::Value;
const ANY: SocketKind = SocketKind::Any;

const NO_INPUTS: Arity = Arity::Exact(&[]);
const ONE_F64: Arity = Arity::Exact(&[F64]);
const TWO_F64: Arity = Arity::Exact(&[F64, F64]);
const ONE_CLK: Arity = Arity::Exact(&[CLK]);
const VALUE_AND_CLOCK: Arity = Arity::Exact(&[F64, CLK]);
const ONE_ANY: Arity = Arity::Exact(&[ANY]);
const ONE_VALUE: Arity = Arity::Exact(&[VAL]);

pub const ALL: &[OpDescriptor] = &[
    // Clocks
    OpDescriptor {
        kind: NodeSpecKind::FixedRate,
        label: "Fixed Rate",
        category: "Clock",
        inputs: NO_INPUTS,
        output: CLK,
        default_spec: || NodeSpec::FixedRate { hz: 100.0 },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::ClockOf,
        label: "Clock Of",
        category: "Clock",
        inputs: ONE_ANY,
        output: CLK,
        default_spec: || NodeSpec::ClockOf,
        arg_count: 0,
    },
    // Generators
    OpDescriptor {
        kind: NodeSpecKind::Waveform,
        label: "Waveform",
        category: "Generator",
        inputs: ONE_CLK,
        output: F64,
        default_spec: || NodeSpec::Waveform {
            shape: Waveform::Sin,
            freq: 1.0,
            amplitude: 1.0,
            phase: 0.0,
            dtype: PrimType::F64,
            out_shape: SmallVec::new(),
        },
        // shape, freq, amplitude, phase, dtype, out_shape
        arg_count: 6,
    },
    OpDescriptor {
        kind: NodeSpecKind::Random,
        label: "Random",
        category: "Generator",
        inputs: ONE_CLK,
        output: F64,
        default_spec: || NodeSpec::Random {
            seed: 1,
            dtype: PrimType::F64,
            out_shape: SmallVec::new(),
        },
        // seed, dtype, out_shape
        arg_count: 3,
    },
    OpDescriptor {
        kind: NodeSpecKind::Constant,
        label: "Constant",
        category: "Generator",
        inputs: ONE_CLK,
        output: F64,
        default_spec: || NodeSpec::Constant {
            value: TypedScalar::F64(0.0),
            out_shape: SmallVec::new(),
        },
        // value, dtype, out_shape
        arg_count: 3,
    },
    // Derive — Affine family
    OpDescriptor {
        kind: NodeSpecKind::Affine,
        label: "Scale",
        category: "Derive",
        inputs: ONE_F64,
        output: F64,
        default_spec: || NodeSpec::Affine {
            op: AffineOp::Scale,
            k: TypedScalar::F64(1.0),
        },
        arg_count: 2,
    },
    OpDescriptor {
        kind: NodeSpecKind::Affine,
        label: "Offset",
        category: "Derive",
        inputs: ONE_F64,
        output: F64,
        default_spec: || NodeSpec::Affine {
            op: AffineOp::Offset,
            k: TypedScalar::F64(0.0),
        },
        arg_count: 2,
    },
    // Derive — Unary family
    OpDescriptor {
        kind: NodeSpecKind::Unary,
        label: "Abs",
        category: "Derive",
        inputs: ONE_F64,
        output: F64,
        default_spec: || NodeSpec::Unary { op: UnaryOp::Abs },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Unary,
        label: "Neg",
        category: "Derive",
        inputs: ONE_F64,
        output: F64,
        default_spec: || NodeSpec::Unary { op: UnaryOp::Neg },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Unary,
        label: "Log",
        category: "Derive",
        inputs: ONE_F64,
        output: F64,
        default_spec: || NodeSpec::Unary { op: UnaryOp::Log },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Unary,
        label: "Sqrt",
        category: "Derive",
        inputs: ONE_F64,
        output: F64,
        default_spec: || NodeSpec::Unary { op: UnaryOp::Sqrt },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Unary,
        label: "Exp",
        category: "Derive",
        inputs: ONE_F64,
        output: F64,
        default_spec: || NodeSpec::Unary { op: UnaryOp::Exp },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Unary,
        label: "Floor",
        category: "Derive",
        inputs: ONE_F64,
        output: F64,
        default_spec: || NodeSpec::Unary { op: UnaryOp::Floor },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Window,
        label: "Window",
        category: "Derive",
        inputs: ONE_F64,
        output: VAL,
        default_spec: || NodeSpec::Window { size: 64 },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Fft,
        label: "FFT",
        category: "Derive",
        inputs: ONE_VALUE,
        output: VAL,
        default_spec: || NodeSpec::Fft,
        arg_count: 0,
    },
    OpDescriptor {
        kind: NodeSpecKind::Magnitude,
        label: "Magnitude",
        category: "Derive",
        inputs: ONE_VALUE,
        output: F64,
        default_spec: || NodeSpec::Magnitude,
        arg_count: 0,
    },
    OpDescriptor {
        kind: NodeSpecKind::Index,
        label: "Index",
        category: "Derive",
        inputs: ONE_VALUE,
        output: F64,
        default_spec: || NodeSpec::Index { index: 0 },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Threshold,
        label: "Threshold",
        category: "Derive",
        inputs: ONE_VALUE,
        output: F64,
        default_spec: || NodeSpec::Threshold {
            k: TypedScalar::F64(0.0),
            op: ThresholdOp::Gt,
        },
        // k, op
        arg_count: 2,
    },
    // Compose — Binary family
    OpDescriptor {
        kind: NodeSpecKind::Binary,
        label: "Add",
        category: "Compose",
        inputs: TWO_F64,
        output: F64,
        default_spec: || NodeSpec::Binary { op: BinaryOp::Add },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Binary,
        label: "Sub",
        category: "Compose",
        inputs: TWO_F64,
        output: F64,
        default_spec: || NodeSpec::Binary { op: BinaryOp::Sub },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Binary,
        label: "Mul",
        category: "Compose",
        inputs: TWO_F64,
        output: F64,
        default_spec: || NodeSpec::Binary { op: BinaryOp::Mul },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Binary,
        label: "Div",
        category: "Compose",
        inputs: TWO_F64,
        output: F64,
        default_spec: || NodeSpec::Binary { op: BinaryOp::Div },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Mean,
        label: "Mean",
        category: "Compose",
        inputs: Arity::Variadic { kind: F64, min: 1 },
        output: F64,
        default_spec: || NodeSpec::Mean,
        arg_count: 0,
    },
    OpDescriptor {
        kind: NodeSpecKind::Pack,
        label: "Pack",
        category: "Compose",
        inputs: Arity::Variadic { kind: F64, min: 1 },
        output: VAL,
        default_spec: || NodeSpec::Pack,
        arg_count: 0,
    },
    OpDescriptor {
        kind: NodeSpecKind::Dot,
        label: "Dot Product",
        category: "Compose",
        inputs: Arity::Exact(&[VAL, VAL]),
        output: F64,
        default_spec: || NodeSpec::Dot,
        arg_count: 0,
    },
    // Resample family
    OpDescriptor {
        kind: NodeSpecKind::Resample,
        label: "Zero-Order Hold",
        category: "Resample",
        inputs: VALUE_AND_CLOCK,
        output: F64,
        default_spec: || NodeSpec::Resample {
            mode: ResampleMode::Zoh,
        },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Resample,
        label: "Linear Interp",
        category: "Resample",
        inputs: VALUE_AND_CLOCK,
        output: F64,
        default_spec: || NodeSpec::Resample {
            mode: ResampleMode::Linear,
        },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Resample,
        label: "Latest At",
        category: "Resample",
        inputs: VALUE_AND_CLOCK,
        output: F64,
        default_spec: || NodeSpec::Resample {
            mode: ResampleMode::LatestAt,
        },
        arg_count: 1,
    },
    // DB bridges
    OpDescriptor {
        kind: NodeSpecKind::FromDb,
        label: "From DB",
        category: "DB",
        inputs: NO_INPUTS,
        output: F64,
        default_spec: || NodeSpec::FromDb { component_id: 0 },
        arg_count: 1,
    },
    OpDescriptor {
        kind: NodeSpecKind::Persist,
        label: "Persist",
        category: "DB",
        inputs: ONE_VALUE,
        output: VAL,
        default_spec: || NodeSpec::Persist {
            name: String::new(),
        },
        arg_count: 1,
    },
];

/// First descriptor with this `kind`. For family kinds (`Binary`/`Unary`/
/// `Affine`/`Resample`) this returns whichever variant happens to be listed
/// first in [`ALL`]; prefer [`descriptor_for`] when you have an actual spec.
pub fn descriptor(kind: NodeSpecKind) -> &'static OpDescriptor {
    ALL.iter()
        .find(|d| d.kind == kind)
        .expect("every NodeSpecKind has a descriptor")
}

/// Find the descriptor that corresponds to `spec`. For family variants this
/// disambiguates by inner op-discriminant (so `Binary { Add }` resolves to
/// the "Add" descriptor, not "Sub"). Falls back to the first descriptor of
/// the spec's kind if no descriptor matches the inner op.
pub fn descriptor_for(spec: &NodeSpec) -> &'static OpDescriptor {
    let kind = spec.kind();
    let inner = spec.family_op_id();
    ALL.iter()
        .find(|d| d.kind == kind && (d.default_spec)().family_op_id() == inner)
        .or_else(|| ALL.iter().find(|d| d.kind == kind))
        .expect("every NodeSpec resolves to a descriptor")
}
