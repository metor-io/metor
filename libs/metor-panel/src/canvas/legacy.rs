//! The node editor's saved format, kept for exactly one purpose: reading it.
//!
//! The node editor is gone — its canvas, its op registry, its per-op inspector
//! rows, and the twenty-one constructors behind them. What could not go with
//! it is the *shape on disk*, because presets saved before it went still hold
//! graphs, and a format nobody can read is a format that silently loses data.
//!
//! So this is the serde surface and nothing else. There is no `build`, no
//! `op_tag`, no `hash_args`: a `NodeSpec` here describes what a node *was*, so
//! that [`super::migrate`] can say what it *is*. The moment a saved layout has
//! been converted, nothing reads these types again — and when no layout in the
//! wild still names them, this file goes too.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::dynamic::ops::resample::ResampleMode;
use crate::dynamic::tensor::TypedScalar;
use metor_proto::types::PrimType;

/// One saved node editor pane.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeEditorConfig {
    pub viewport: Viewport,
    pub nodes: Vec<SerializedNode>,
    pub edges: Vec<SerializedEdge>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SerializedNode {
    pub flow_id: String,
    pub spec: NodeSpec,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SerializedEdge {
    pub source: String,
    pub target: String,
    pub target_socket: u32,
}

/// `x * k` or `x + k`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AffineOp {
    Scale,
    Offset,
}

/// Single-input math.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnaryOp {
    Abs,
    Neg,
    Log,
    Sqrt,
    Exp,
    Floor,
}

/// A comparison that published `1.0` or `0.0` — not a bool, which is why the
/// conversion is a conditional rather than a comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThresholdOp {
    Gt,
    Ge,
    Lt,
    Le,
}

/// Two-input elementwise arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// The periodic shapes the generator node offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Waveform {
    Sin,
    Cos,
    Square,
    Sawtooth,
}

/// What one saved node was.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeSpec {
    FixedRate {
        hz: f64,
    },
    ClockOf,
    Waveform {
        shape: Waveform,
        freq: f64,
        amplitude: f64,
        phase: f64,
        dtype: PrimType,
        out_shape: SmallVec<[usize; 4]>,
    },
    Random {
        seed: u64,
        dtype: PrimType,
        out_shape: SmallVec<[usize; 4]>,
    },
    Constant {
        value: TypedScalar,
        out_shape: SmallVec<[usize; 4]>,
    },
    Affine {
        op: AffineOp,
        k: TypedScalar,
    },
    Unary {
        op: UnaryOp,
    },
    Window {
        size: usize,
    },
    Fft,
    Magnitude,
    Index {
        index: usize,
    },
    Threshold {
        k: TypedScalar,
        op: ThresholdOp,
    },
    Delta,
    DeltaT,
    Binary {
        op: BinaryOp,
    },
    Mean,
    Pack,
    Dot,
    Resample {
        mode: ResampleMode,
    },
    FromDb {
        component_id: u64,
    },
    Persist {
        name: String,
    },
}
