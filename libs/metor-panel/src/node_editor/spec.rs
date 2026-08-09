//! `NodeSpec`: serializable description of a single node in the graph.
//!
//! A `NodeSpec` carries only the *args* of an op; parent node ids come from
//! graph edges and are mixed into the [`NodeId`] hash by [`compute_node_id`].
//! Every variant's `op_tag()` and `hash_args()` mirror the corresponding
//! constructor in `crate::dynamic::ops::*` byte-for-byte — drift here breaks
//! reconciliation silently (every rebuild recreates everything; in-flight
//! tasks die). The `tests::node_id_matches_constructor` test enforces this.

use std::hash::Hash;
use std::sync::Arc;

use metor_db::DB;
use metor_proto::types::{ComponentId, PrimType};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::dynamic::ops::compose::BinaryOp;
use crate::dynamic::ops::derive::{AffineOp, ThresholdOp, UnaryOp};
use crate::dynamic::ops::generators::Waveform;
use crate::dynamic::ops::resample::ResampleMode;
use crate::dynamic::tensor::TypedScalar;
use crate::dynamic::{BuildError, DynamicNode, NodeId, hash_id, ops};

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
    /// Affine-with-constant: `x * k` (Scale) or `x + k` (Offset).
    Affine {
        op: AffineOp,
        k: TypedScalar,
    },
    /// Single-input unary math: Abs/Neg/Log/Sqrt/Exp/Floor.
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
    /// First-difference: `x[n] - x[n-1]`, element-wise, output dtype `f64`.
    Delta,
    /// Time-difference between consecutive samples, in seconds, as an `f64` scalar.
    DeltaT,
    /// Two-input element-wise arithmetic: Add/Sub/Mul/Div.
    Binary {
        op: BinaryOp,
    },
    Mean,
    Pack,
    Dot,
    /// Resample onto a clock: zero-order hold or linear interpolation.
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

/// Compact discriminant used to key things off a spec without comparing args.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeSpecKind {
    FixedRate,
    ClockOf,
    Waveform,
    Random,
    Constant,
    Affine,
    Unary,
    Window,
    Fft,
    Magnitude,
    Index,
    Threshold,
    Delta,
    DeltaT,
    Binary,
    Mean,
    Pack,
    Dot,
    Resample,
    FromDb,
    Persist,
}

impl NodeSpec {
    pub fn kind(&self) -> NodeSpecKind {
        use NodeSpec::*;
        match self {
            FixedRate { .. } => NodeSpecKind::FixedRate,
            ClockOf => NodeSpecKind::ClockOf,
            Waveform { .. } => NodeSpecKind::Waveform,
            Random { .. } => NodeSpecKind::Random,
            Constant { .. } => NodeSpecKind::Constant,
            Affine { .. } => NodeSpecKind::Affine,
            Unary { .. } => NodeSpecKind::Unary,
            Window { .. } => NodeSpecKind::Window,
            Fft => NodeSpecKind::Fft,
            Magnitude => NodeSpecKind::Magnitude,
            Index { .. } => NodeSpecKind::Index,
            Threshold { .. } => NodeSpecKind::Threshold,
            Delta => NodeSpecKind::Delta,
            DeltaT => NodeSpecKind::DeltaT,
            Binary { .. } => NodeSpecKind::Binary,
            Mean => NodeSpecKind::Mean,
            Pack => NodeSpecKind::Pack,
            Dot => NodeSpecKind::Dot,
            Resample { .. } => NodeSpecKind::Resample,
            FromDb { .. } => NodeSpecKind::FromDb,
            Persist { .. } => NodeSpecKind::Persist,
        }
    }

    /// Inner op-discriminant for family variants (`Affine`/`Unary`/`Binary`/
    /// `Resample`). Used by the editor registry to disambiguate descriptors
    /// that share a [`NodeSpecKind`]. Returns `None` for non-family variants.
    pub fn family_op_id(&self) -> Option<u8> {
        use NodeSpec::*;
        match self {
            Affine { op, .. } => Some(*op as u8),
            Unary { op } => Some(*op as u8),
            Binary { op } => Some(*op as u8),
            Resample { mode } => Some(*mode as u8),
            _ => None,
        }
    }

    pub fn op_tag(&self) -> &'static [u8] {
        use crate::dynamic::op_tag;
        use NodeSpec::*;
        match self {
            FixedRate { .. } => op_tag::FIXED_RATE_CLOCK,
            ClockOf => op_tag::CLOCK_OF,
            Waveform { .. } => op_tag::WAVEFORM,
            Random { .. } => op_tag::RANDOM,
            Constant { .. } => op_tag::CONSTANT,
            Affine { op, .. } => op.op_tag(),
            Unary { op } => op.op_tag(),
            Window { .. } => op_tag::WINDOW,
            Fft => op_tag::FFT,
            Magnitude => op_tag::MAGNITUDE,
            Index { .. } => op_tag::INDEX,
            Threshold { .. } => op_tag::THRESHOLD,
            Delta => op_tag::DELTA,
            DeltaT => op_tag::DELTA_T,
            Binary { op } => op.op_tag(),
            Mean => op_tag::MEAN,
            Pack => op_tag::PACK,
            Dot => op_tag::DOT,
            Resample { mode } => mode.op_tag(),
            FromDb { .. } => op_tag::FROM_DB,
            Persist { .. } => op_tag::PERSIST,
        }
    }

    /// Mirror the `args` closure each constructor passes to
    /// [`hash_id`](crate::dynamic::node::hash_id). Drift breaks reconciliation.
    /// The op-discriminant of family variants (`Affine`/`Unary`/`Binary`/
    /// `Resample`) is encoded in `op_tag()` and intentionally not mixed in
    /// here — keeping IDs stable across the family-consolidation refactor.
    fn hash_args(&self, h: &mut std::collections::hash_map::DefaultHasher) {
        use NodeSpec::*;
        match self {
            FixedRate { hz } => {
                hz.to_bits().hash(h);
            }
            ClockOf => {}
            Waveform {
                shape,
                freq,
                amplitude,
                phase,
                dtype,
                out_shape,
            } => {
                (*shape as u8).hash(h);
                freq.to_bits().hash(h);
                amplitude.to_bits().hash(h);
                phase.to_bits().hash(h);
                (*dtype as u8).hash(h);
                for d in out_shape {
                    d.hash(h);
                }
            }
            Random {
                seed,
                dtype,
                out_shape,
            } => {
                seed.hash(h);
                (*dtype as u8).hash(h);
                for d in out_shape {
                    d.hash(h);
                }
            }
            Constant { value, out_shape } => {
                value.hash(h);
                for d in out_shape {
                    d.hash(h);
                }
            }
            Affine { k, .. } => {
                k.hash(h);
            }
            Unary { .. } | Fft | Magnitude | Delta | DeltaT => {}
            Window { size } => {
                size.hash(h);
            }
            Index { index } => {
                index.hash(h);
            }
            Threshold { k, op } => {
                k.hash(h);
                (*op as u8).hash(h);
            }
            Binary { .. } | Mean | Pack | Dot => {}
            Resample { .. } => {}
            FromDb { component_id } => {
                component_id.hash(h);
            }
            Persist { name } => {
                name.hash(h);
            }
        }
    }
}

/// Compute the `NodeId` a built node would have, given its spec and the ids
/// of its parents in the canonical order this op expects.
pub fn compute_node_id(spec: &NodeSpec, parents: &[NodeId]) -> NodeId {
    hash_id(spec.op_tag(), parents, |h| spec.hash_args(h))
}

/// Build a runtime node from a spec and its already-built parents. The caller
/// is responsible for passing parents in the canonical order — same order
/// used by [`compute_node_id`].
pub fn build(
    spec: &NodeSpec,
    parents: Vec<Arc<dyn DynamicNode>>,
    db: &DB,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    use NodeSpec::*;

    fn p1(
        op: &'static str,
        parents: Vec<Arc<dyn DynamicNode>>,
    ) -> Result<Arc<dyn DynamicNode>, BuildError> {
        match <[_; 1]>::try_from(parents) {
            Ok([a]) => Ok(a),
            Err(parents) => Err(BuildError::WrongArity {
                op,
                expected: 1,
                got: parents.len(),
            }),
        }
    }

    fn p2(
        op: &'static str,
        parents: Vec<Arc<dyn DynamicNode>>,
    ) -> Result<(Arc<dyn DynamicNode>, Arc<dyn DynamicNode>), BuildError> {
        match <[_; 2]>::try_from(parents) {
            Ok([a, b]) => Ok((a, b)),
            Err(parents) => Err(BuildError::WrongArity {
                op,
                expected: 2,
                got: parents.len(),
            }),
        }
    }

    fn p0(op: &'static str, parents: Vec<Arc<dyn DynamicNode>>) -> Result<(), BuildError> {
        if !parents.is_empty() {
            return Err(BuildError::WrongArity {
                op,
                expected: 0,
                got: parents.len(),
            });
        }
        Ok(())
    }

    match spec {
        FixedRate { hz } => {
            p0("fixed_rate", parents)?;
            ops::clock::fixed_rate(*hz)
        }
        ClockOf => Ok(ops::clock::clock_of(p1("clock_of", parents)?)),
        Waveform {
            shape,
            freq,
            amplitude,
            phase,
            dtype,
            out_shape,
        } => ops::generators::waveform(
            p1("waveform", parents)?,
            *shape,
            *freq,
            *amplitude,
            *phase,
            *dtype,
            out_shape.clone(),
        ),
        Random {
            seed,
            dtype,
            out_shape,
        } => ops::generators::random(p1("random", parents)?, *seed, *dtype, out_shape.clone()),
        Constant { value, out_shape } => {
            ops::generators::constant(p1("constant", parents)?, *value, out_shape.clone())
        }
        Affine { op, k } => ops::derive::affine(p1("affine", parents)?, *op, *k),
        Unary { op } => ops::derive::unary(p1("unary", parents)?, *op),
        Window { size } => ops::derive::window(p1("window", parents)?, *size),
        Fft => ops::derive::fft(p1("fft", parents)?),
        Magnitude => ops::derive::magnitude(p1("magnitude", parents)?),
        Index { index } => ops::derive::index(p1("index", parents)?, *index),
        Threshold { k, op } => ops::derive::threshold(p1("threshold", parents)?, *k, *op),
        Delta => ops::derive::delta(p1("delta", parents)?),
        DeltaT => ops::derive::delta_t(p1("delta_t", parents)?),
        Binary { op } => {
            let (a, b) = p2("binary", parents)?;
            ops::compose::binary_op(a, b, *op)
        }
        Mean => {
            if parents.is_empty() {
                return Err(BuildError::EmptyInputs);
            }
            ops::compose::mean(parents)
        }
        Pack => {
            if parents.is_empty() {
                return Err(BuildError::EmptyInputs);
            }
            ops::compose::pack(parents)
        }
        Dot => {
            let (a, b) = p2("dot", parents)?;
            ops::compose::dot(a, b)
        }
        Resample { mode } => {
            let (input, clock) = p2("resample", parents)?;
            ops::resample::resample(input, clock, *mode)
        }
        FromDb { component_id } => {
            p0("from_db", parents)?;
            ops::db_source::from_db(db, ComponentId(*component_id))
        }
        Persist { name } => ops::persist::persist(db, name.clone(), p1("persist", parents)?),
    }
}
