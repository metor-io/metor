//! Multi-input value composers. Inputs **must share a clock**
//! (`parent_clock_id`); use the resampler ops to align mismatched rates.
//!
//! Co-clocked inputs have aligned timestamps, so the composer task pulls one
//! grant from each reader, processes `min(grant_lens)` aligned tuples in
//! lockstep, and lets any remaining samples in longer grants drop on release.
//! Per-element compute happens in `f64`; output dtype follows NumPy promotion
//! across inputs and shapes broadcast NumPy-style.

use std::mem::size_of;
use std::sync::Arc;

use metor_db::{ComponentSchema, disruptor::Disruptor};
use metor_proto::types::{PrimType, Timestamp};
use smallvec::SmallVec;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeId, NodeImpl, NodeReader, ValueType,
    default_ring_bytes, hash_id, op_tag, require_value,
};
use crate::dynamic::tensor::{
    BroadcastIter, broadcast_shape, broadcast_shape_many, promote, promote_many, read_f64_at,
    shape_elems, write_f64_at,
};

fn require_same_clock(nodes: &[Arc<dyn DynamicNode>]) -> Result<NodeId, BuildError> {
    let first = nodes.first().ok_or(BuildError::EmptyInputs)?;
    let clock = first.parent_clock_id().ok_or(BuildError::ClockMismatch)?;
    for node in &nodes[1..] {
        if node.parent_clock_id() != Some(clock) {
            return Err(BuildError::ClockMismatch);
        }
    }
    Ok(clock)
}

/// Align N co-clocked byte streams by timestamp. For each aligned tuple, calls
/// `encode` with the per-input `&[u8]` slices (in input order) and a
/// `&mut [u8]` slot of `out_value_size` bytes carved out of the output ring's
/// `WriteGrant`.
///
/// Inputs share a clock, so in steady state their heads carry equal
/// timestamps. A skew — one reader subscribing a sample late, a stall, a slow
/// consumer — must not wedge the composer: positional pairing would then
/// mismatch every future tuple and emit nothing forever. Instead we hold a
/// per-input cursor and, whenever the heads disagree, advance only the
/// stream(s) whose head is *older* than the newest, discarding the stale
/// sample and re-comparing until the heads line up. Leftover samples past the
/// end of the shorter grant drop on release — lossy by design, no buffering.
pub(crate) async fn run_aligned_emit(
    mut readers: Vec<NodeReader>,
    output: Disruptor,
    out_value_size: usize,
    mut encode: impl FnMut(&[&[u8]], &mut [u8]),
) {
    let n = readers.len();
    let out_msg_size = size_of::<Timestamp>() + out_value_size;
    loop {
        // Sequentially await one grant from each reader. Each future borrows
        // a distinct &mut NodeReader, so the held grants coexist.
        let mut grants = Vec::with_capacity(n);
        for r in readers.iter_mut() {
            grants.push(r.next().await);
        }
        let counts: SmallVec<[usize; 4]> = grants.iter().map(|g| g.sample_count()).collect();
        let mut cursors: SmallVec<[usize; 4]> = SmallVec::from_elem(0usize, n);
        // `tuple` holds refs into `grants`; both live for this outer iteration.
        let mut tuple: SmallVec<[&[u8]; 4]> = SmallVec::with_capacity(n);
        loop {
            if cursors.iter().zip(&counts).any(|(cur, count)| cur >= count) {
                break;
            }
            let newest = (0..n)
                .map(|k| grants[k].sample_at(cursors[k]).0)
                .max_by_key(|ts| ts.0)
                .expect("at least one input");
            // Drop any head that predates `newest` and re-compare next pass.
            let mut advanced = false;
            for k in 0..n {
                if grants[k].sample_at(cursors[k]).0.0 < newest.0 {
                    cursors[k] += 1;
                    advanced = true;
                }
            }
            if advanced {
                continue;
            }
            tuple.clear();
            for k in 0..n {
                tuple.push(grants[k].sample_at(cursors[k]).1);
                cursors[k] += 1;
            }
            let Ok(mut wg) = output.try_grant(out_msg_size) else {
                continue;
            };
            wg[..size_of::<Timestamp>()].copy_from_slice(&newest.to_le_bytes());
            encode(&tuple, &mut wg[size_of::<Timestamp>()..]);
        }
        // Grants drop here; their full ranges are consumed.
    }
}

/// Broadcast-aware elementwise binary op. `op` runs in `f64`.
fn binary(
    tag: &'static [u8],
    a: Arc<dyn DynamicNode>,
    b: Arc<dyn DynamicNode>,
    op: impl Fn(f64, f64) -> f64 + Send + Sync + 'static,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let a_schema = require_value(&a)?;
    let b_schema = require_value(&b)?;
    let clock = require_same_clock(&[a.clone(), b.clone()])?;
    let out_dtype = promote(a_schema.prim_type, b_schema.prim_type);
    let out_dim = broadcast_shape(&a_schema.dim, &b_schema.dim)?;
    let out_schema = ComponentSchema::new(out_dtype, &out_dim);
    let id = hash_id(tag, &[a.id(), b.id()], |_| {});
    let readers = vec![a.subscribe(), b.subscribe()];
    let a_dtype = a_schema.prim_type;
    let b_dtype = b_schema.prim_type;
    let a_dim_clone: SmallVec<[usize; 4]> = a_schema.dim.clone();
    let b_dim_clone: SmallVec<[usize; 4]> = b_schema.dim.clone();
    let out_dim_clone: SmallVec<[usize; 4]> = out_dim.clone();
    let out_value_size = out_schema.size();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema),
        Some(clock),
        default_ring_bytes(out_value_size),
        move |output| async move {
            let _a = a;
            let _b = b;
            run_aligned_emit(readers, output, out_value_size, move |inputs, out| {
                let av = inputs[0];
                let bv = inputs[1];
                let it_a = BroadcastIter::new(&a_dim_clone, &out_dim_clone);
                let it_b = BroadcastIter::new(&b_dim_clone, &out_dim_clone);
                for (i, (ai, bi)) in it_a.zip(it_b).enumerate() {
                    let x = read_f64_at(av, a_dtype, ai);
                    let y = read_f64_at(bv, b_dtype, bi);
                    write_f64_at(out, out_dtype, i, op(x, y));
                }
            })
            .await;
        },
    ))
}

/// Element-wise binary arithmetic operator. Each variant is a separate node
/// at runtime — the `op_tag` is per-variant so existing serialized graphs
/// hash to the same `NodeId` after the consolidation refactor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl BinaryOp {
    pub fn op_tag(self) -> &'static [u8] {
        match self {
            BinaryOp::Add => op_tag::ADD,
            BinaryOp::Sub => op_tag::SUB,
            BinaryOp::Mul => op_tag::MUL,
            BinaryOp::Div => op_tag::DIV,
        }
    }

    fn apply(self, x: f64, y: f64) -> f64 {
        match self {
            BinaryOp::Add => x + y,
            BinaryOp::Sub => x - y,
            BinaryOp::Mul => x * y,
            BinaryOp::Div => x / y,
        }
    }
}

pub fn binary_op(
    a: Arc<dyn DynamicNode>,
    b: Arc<dyn DynamicNode>,
    op: BinaryOp,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    binary(op.op_tag(), a, b, move |x, y| op.apply(x, y))
}

/// Element-wise mean of N co-clocked inputs. Output dtype is the float-wide
/// promotion of inputs (mean of ints → `f64`); shape is broadcast across all.
pub fn mean(inputs: Vec<Arc<dyn DynamicNode>>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    if inputs.is_empty() {
        return Err(BuildError::EmptyInputs);
    }
    let schemas: Vec<ComponentSchema> =
        inputs.iter().map(require_value).collect::<Result<_, _>>()?;
    let clock = require_same_clock(&inputs)?;
    // Float-promote so int/int produces a float (matches NumPy's mean).
    let mut promoted = promote_many(schemas.iter().map(|s| s.prim_type));
    if !crate::dynamic::tensor::is_float(promoted) {
        promoted = PrimType::F64;
    }
    let out_dim = broadcast_shape_many(schemas.iter().map(|s| s.dim.clone()))?;
    let out_schema = ComponentSchema::new(promoted, &out_dim);
    let parent_ids: Vec<NodeId> = inputs.iter().map(|n| n.id()).collect();
    let id = hash_id(op_tag::MEAN, &parent_ids, |_| {});
    let n_inputs = inputs.len();
    let inv_n = 1.0 / n_inputs as f64;
    let readers: Vec<NodeReader> = inputs.iter().map(|n| n.subscribe()).collect();
    let in_dims: Vec<SmallVec<[usize; 4]>> = schemas.iter().map(|s| s.dim.clone()).collect();
    let in_dtypes: Vec<PrimType> = schemas.iter().map(|s| s.prim_type).collect();
    let out_dim_clone = out_dim.clone();
    let out_total = shape_elems(&out_dim);
    let out_value_size = out_schema.size();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema),
        Some(clock),
        default_ring_bytes(out_value_size),
        move |output| async move {
            let _inputs = inputs;
            let mut accum: Vec<f64> = vec![0.0; out_total];
            run_aligned_emit(readers, output, out_value_size, move |inputs_bytes, out| {
                for v in accum.iter_mut() {
                    *v = 0.0;
                }
                for k in 0..n_inputs {
                    let bytes = inputs_bytes[k];
                    let it = BroadcastIter::new(&in_dims[k], &out_dim_clone);
                    for (out_idx, src_idx) in it.enumerate() {
                        accum[out_idx] += read_f64_at(bytes, in_dtypes[k], src_idx);
                    }
                }
                for (i, v) in accum.iter().enumerate() {
                    write_f64_at(out, promoted, i, *v * inv_n);
                }
            })
            .await;
        },
    ))
}

/// Dot product of two co-clocked vector inputs — `sum(a_i * b_i)` over the
/// (broadcast) common shape, reduced to a rank-0 scalar.
///
/// The output is float-promoted regardless of input dtype because the sum
/// of int products can easily overflow narrow int types (matches NumPy's
/// `np.dot` widening for ints).
pub fn dot(
    a: Arc<dyn DynamicNode>,
    b: Arc<dyn DynamicNode>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let a_schema = require_value(&a)?;
    let b_schema = require_value(&b)?;
    if a_schema.prim_type == PrimType::Bool || b_schema.prim_type == PrimType::Bool {
        return Err(BuildError::UnsupportedDtype {
            op: "dot",
            dtype: PrimType::Bool,
        });
    }
    let clock = require_same_clock(&[a.clone(), b.clone()])?;
    let common_shape = broadcast_shape(&a_schema.dim, &b_schema.dim)?;
    let mut out_dtype = promote(a_schema.prim_type, b_schema.prim_type);
    if !crate::dynamic::tensor::is_float(out_dtype) {
        out_dtype = PrimType::F64;
    }
    let out_schema = ComponentSchema::new(out_dtype, &[]);
    let id = hash_id(op_tag::DOT, &[a.id(), b.id()], |_| {});
    let readers = vec![a.subscribe(), b.subscribe()];
    let a_dtype = a_schema.prim_type;
    let b_dtype = b_schema.prim_type;
    let a_dim_clone: SmallVec<[usize; 4]> = a_schema.dim.clone();
    let b_dim_clone: SmallVec<[usize; 4]> = b_schema.dim.clone();
    let common_shape_clone = common_shape.clone();
    let out_value_size = out_schema.size();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema),
        Some(clock),
        default_ring_bytes(out_value_size),
        move |output| async move {
            let _a = a;
            let _b = b;
            run_aligned_emit(readers, output, out_value_size, move |inputs, out| {
                let av = inputs[0];
                let bv = inputs[1];
                let it_a = BroadcastIter::new(&a_dim_clone, &common_shape_clone);
                let it_b = BroadcastIter::new(&b_dim_clone, &common_shape_clone);
                let mut sum: f64 = 0.0;
                for (ai, bi) in it_a.zip(it_b) {
                    let x = read_f64_at(av, a_dtype, ai);
                    let y = read_f64_at(bv, b_dtype, bi);
                    sum += x * y;
                }
                write_f64_at(out, out_dtype, 0, sum);
            })
            .await;
        },
    ))
}

/// Pack N co-clocked values into a single component with a leading length-N
/// axis. Per-input shapes broadcast against each other; dtype promotes across
/// inputs.
pub fn pack(inputs: Vec<Arc<dyn DynamicNode>>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    if inputs.is_empty() {
        return Err(BuildError::EmptyInputs);
    }
    let schemas: Vec<ComponentSchema> =
        inputs.iter().map(require_value).collect::<Result<_, _>>()?;
    let clock = require_same_clock(&inputs)?;
    let promoted = promote_many(schemas.iter().map(|s| s.prim_type));
    let inner_dim = broadcast_shape_many(schemas.iter().map(|s| s.dim.clone()))?;
    let n = inputs.len();
    let mut out_dim: SmallVec<[usize; 4]> = SmallVec::with_capacity(inner_dim.len() + 1);
    out_dim.push(n);
    out_dim.extend(inner_dim.iter().copied());
    let out_schema = ComponentSchema::new(promoted, &out_dim);
    let parent_ids: Vec<NodeId> = inputs.iter().map(|n| n.id()).collect();
    let id = hash_id(op_tag::PACK, &parent_ids, |_| {});
    let readers: Vec<NodeReader> = inputs.iter().map(|n| n.subscribe()).collect();
    let in_dims: Vec<SmallVec<[usize; 4]>> = schemas.iter().map(|s| s.dim.clone()).collect();
    let in_dtypes: Vec<PrimType> = schemas.iter().map(|s| s.prim_type).collect();
    let inner_dim_clone = inner_dim.clone();
    let inner_size = shape_elems(&inner_dim);
    let out_value_size = out_schema.size();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema),
        Some(clock),
        default_ring_bytes(out_value_size),
        move |output| async move {
            let _inputs = inputs;
            run_aligned_emit(readers, output, out_value_size, move |inputs_bytes, out| {
                for k in 0..n {
                    let bytes = inputs_bytes[k];
                    let base = k * inner_size;
                    let it = BroadcastIter::new(&in_dims[k], &inner_dim_clone);
                    for (j, src_idx) in it.enumerate() {
                        let v = read_f64_at(bytes, in_dtypes[k], src_idx);
                        write_f64_at(out, promoted, base + j, v);
                    }
                }
            })
            .await;
        },
    ))
}
