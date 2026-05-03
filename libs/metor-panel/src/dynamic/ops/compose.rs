//! Multi-input value composers. Inputs **must share a clock**
//! (`parent_clock_id`); use the resampler ops to align mismatched rates.
//!
//! Co-clocked inputs have aligned timestamps, so the composer task pulls one
//! sample from each, asserts the timestamps match, and emits a combined
//! value. Per-element compute happens in `f64`; output dtype follows NumPy
//! promotion across inputs and shapes broadcast NumPy-style.

use std::collections::VecDeque;
use std::sync::Arc;

use metor_db::{ComponentSchema, disruptor::Disruptor};
use metor_proto::types::{PrimType, Timestamp};
use smallvec::SmallVec;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeId, NodeImpl, NodeReader, ValueType,
    default_ring_bytes, hash_id, op_tag, require_value, write_sample,
};
use crate::dynamic::tensor::{
    BroadcastIter, broadcast_shape, broadcast_shape_many, promote, promote_many, read_f64_at,
    shape_elems, write_f64_as,
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

/// Align N co-clocked byte streams. For each aligned tuple, calls `encode`
/// with the per-input `&[u8]` slices (in input order) to fill the output
/// sample bytes. Heads desyncs drop oldest until aligned.
async fn run_aligned_emit(
    mut readers: Vec<NodeReader>,
    output: Disruptor,
    mut encode: impl FnMut(&[&[u8]], &mut Vec<u8>),
) {
    let n = readers.len();
    let mut bufs: Vec<VecDeque<(Timestamp, Vec<u8>)>> =
        (0..n).map(|_| VecDeque::new()).collect();
    let mut scratch: Vec<u8> = Vec::new();
    loop {
        for (i, reader) in readers.iter_mut().enumerate() {
            if bufs[i].is_empty() {
                let grant = reader.next().await;
                for (ts, v) in grant.samples() {
                    bufs[i].push_back((ts, v.to_vec()));
                }
            }
        }
        realign_heads(&mut bufs);
        while let Some(ts) = aligned_head(&bufs) {
            // Pop fronts so we own the bytes for the encode pass.
            let owned: Vec<Vec<u8>> = (0..n)
                .map(|i| bufs[i].pop_front().expect("aligned head").1)
                .collect();
            let tuple: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
            scratch.clear();
            encode(&tuple, &mut scratch);
            write_sample(&output, ts, &scratch);
        }
    }
}

fn aligned_head(bufs: &[VecDeque<(Timestamp, Vec<u8>)>]) -> Option<Timestamp> {
    let ts0 = bufs.first()?.front()?.0;
    bufs.iter()
        .all(|b| b.front().map(|(ts, _)| *ts) == Some(ts0))
        .then_some(ts0)
}

fn realign_heads(bufs: &mut [VecDeque<(Timestamp, Vec<u8>)>]) {
    loop {
        let mut min_ts: Option<Timestamp> = None;
        let mut max_ts: Option<Timestamp> = None;
        for buf in bufs.iter() {
            let Some((ts, _)) = buf.front() else {
                return;
            };
            let ts = *ts;
            min_ts = Some(min_ts.map_or(ts, |m| if ts.0 < m.0 { ts } else { m }));
            max_ts = Some(max_ts.map_or(ts, |m| if ts.0 > m.0 { ts } else { m }));
        }
        let (Some(min_ts), Some(max_ts)) = (min_ts, max_ts) else {
            return;
        };
        if min_ts == max_ts {
            return;
        }
        tracing::warn!(?min_ts, ?max_ts, "compose: timestamps drifted; dropping older sample(s)");
        for buf in bufs.iter_mut() {
            if let Some((ts, _)) = buf.front()
                && ts.0 < max_ts.0
            {
                buf.pop_front();
            }
        }
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
    let a_size = a_schema.size();
    let b_size = b_schema.size();
    let a_dtype = a_schema.prim_type;
    let b_dtype = b_schema.prim_type;
    let a_dim_clone: SmallVec<[usize; 4]> = a_schema.dim.clone();
    let b_dim_clone: SmallVec<[usize; 4]> = b_schema.dim.clone();
    let out_dim_clone: SmallVec<[usize; 4]> = out_dim.clone();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        Some(clock),
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _a = a;
            let _b = b;
            run_aligned_emit(readers, output, move |inputs, out| {
                let av = inputs[0];
                let bv = inputs[1];
                if av.len() != a_size || bv.len() != b_size {
                    return;
                }
                let it_a = BroadcastIter::new(&a_dim_clone, &out_dim_clone);
                let it_b = BroadcastIter::new(&b_dim_clone, &out_dim_clone);
                for (ai, bi) in it_a.zip(it_b) {
                    let x = read_f64_at(av, a_dtype, ai);
                    let y = read_f64_at(bv, b_dtype, bi);
                    write_f64_as(out, out_dtype, op(x, y));
                }
            })
            .await;
        },
    ))
}

pub fn add(a: Arc<dyn DynamicNode>, b: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    binary(op_tag::ADD, a, b, |x, y| x + y)
}

pub fn sub(a: Arc<dyn DynamicNode>, b: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    binary(op_tag::SUB, a, b, |x, y| x - y)
}

pub fn mul(a: Arc<dyn DynamicNode>, b: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    binary(op_tag::MUL, a, b, |x, y| x * y)
}

/// Element-wise mean of N co-clocked inputs. Output dtype is the float-wide
/// promotion of inputs (mean of ints → `f64`); shape is broadcast across all.
pub fn mean(inputs: Vec<Arc<dyn DynamicNode>>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    if inputs.is_empty() {
        return Err(BuildError::EmptyInputs);
    }
    let schemas: Vec<ComponentSchema> = inputs.iter().map(require_value).collect::<Result<_, _>>()?;
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
    let in_sizes: Vec<usize> = schemas.iter().map(|s| s.size()).collect();
    let out_dim_clone = out_dim.clone();
    let out_total = shape_elems(&out_dim);
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        Some(clock),
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _inputs = inputs;
            let mut accum: Vec<f64> = vec![0.0; out_total];
            run_aligned_emit(readers, output, move |inputs_bytes, out| {
                for v in accum.iter_mut() {
                    *v = 0.0;
                }
                for k in 0..n_inputs {
                    let bytes = inputs_bytes[k];
                    if bytes.len() != in_sizes[k] {
                        return;
                    }
                    let it = BroadcastIter::new(&in_dims[k], &out_dim_clone);
                    for (out_idx, src_idx) in it.enumerate() {
                        accum[out_idx] += read_f64_at(bytes, in_dtypes[k], src_idx);
                    }
                }
                for v in accum.iter() {
                    write_f64_as(out, promoted, *v * inv_n);
                }
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
    let schemas: Vec<ComponentSchema> = inputs.iter().map(require_value).collect::<Result<_, _>>()?;
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
    let in_sizes: Vec<usize> = schemas.iter().map(|s| s.size()).collect();
    let inner_dim_clone = inner_dim.clone();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        Some(clock),
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _inputs = inputs;
            run_aligned_emit(readers, output, move |inputs_bytes, out| {
                for k in 0..n {
                    let bytes = inputs_bytes[k];
                    if bytes.len() != in_sizes[k] {
                        return;
                    }
                    let it = BroadcastIter::new(&in_dims[k], &inner_dim_clone);
                    for src_idx in it {
                        let v = read_f64_at(bytes, in_dtypes[k], src_idx);
                        write_f64_as(out, promoted, v);
                    }
                }
            })
            .await;
        },
    ))
}
