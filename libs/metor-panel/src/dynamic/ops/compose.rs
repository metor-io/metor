//! Multi-input value composers. Inputs **must share a clock**
//! (`parent_clock_id`); use the resampler ops to align mismatched rates.
//!
//! Co-clocked inputs have aligned timestamps in lockstep, so the composer
//! task pulls one sample from each, asserts the timestamps match, and emits
//! a combined value. Phase 1 scope: f64 scalars.

use std::collections::VecDeque;
use std::sync::Arc;

use metor_db::disruptor::Disruptor;
use metor_proto::types::Timestamp;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeId, NodeImpl, NodeReader, ValueType,
    default_ring_bytes, hash_id, op_tag, require_f64_scalar, write_sample,
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

fn read_f64(value: &[u8]) -> f64 {
    f64::from_le_bytes(value.try_into().expect("f64 scalar"))
}

/// Align N co-clocked f64 streams and call `encode` for each aligned tuple
/// to fill the value bytes of the output sample.
///
/// Producers may drop samples on a full ring; if heads desync we drop the
/// oldest head(s) until they realign rather than emit a value mixed across
/// ticks. Loops forever — caller wraps in `NodeImpl::spawn`.
async fn run_aligned_emit(
    mut readers: Vec<NodeReader>,
    output: Disruptor,
    mut encode: impl FnMut(&[f64], &mut Vec<u8>),
) {
    let n = readers.len();
    let mut bufs: Vec<VecDeque<(Timestamp, f64)>> = (0..n).map(|_| VecDeque::new()).collect();
    let mut tuple = vec![0.0f64; n];
    let mut scratch: Vec<u8> = Vec::new();
    loop {
        for (i, reader) in readers.iter_mut().enumerate() {
            if bufs[i].is_empty() {
                let grant = reader.next().await;
                bufs[i].extend(grant.samples().map(|(ts, v)| (ts, read_f64(v))));
            }
        }
        realign_heads(&mut bufs);
        while let Some(ts) = aligned_head(&bufs) {
            for (i, buf) in bufs.iter_mut().enumerate() {
                tuple[i] = buf.pop_front().expect("aligned head present").1;
            }
            scratch.clear();
            encode(&tuple, &mut scratch);
            write_sample(&output, ts, &scratch);
        }
    }
}

/// If every buffer has a head and they share a timestamp, return it.
fn aligned_head(bufs: &[VecDeque<(Timestamp, f64)>]) -> Option<Timestamp> {
    let ts0 = bufs.first()?.front()?.0;
    bufs.iter()
        .all(|b| b.front().map(|&(ts, _)| ts) == Some(ts0))
        .then_some(ts0)
}

/// Drop heads older than the newest head until either the queue empties or
/// every head shares a timestamp.
fn realign_heads(bufs: &mut [VecDeque<(Timestamp, f64)>]) {
    loop {
        let mut min_ts: Option<Timestamp> = None;
        let mut max_ts: Option<Timestamp> = None;
        for buf in bufs.iter() {
            let Some(&(ts, _)) = buf.front() else {
                return;
            };
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
            if let Some(&(ts, _)) = buf.front()
                && ts.0 < max_ts.0
            {
                buf.pop_front();
            }
        }
    }
}

fn binary(
    tag: &'static [u8],
    a: Arc<dyn DynamicNode>,
    b: Arc<dyn DynamicNode>,
    f: impl Fn(f64, f64) -> f64 + Send + Sync + 'static,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let schema = require_f64_scalar(&a)?;
    let _ = require_f64_scalar(&b)?;
    let clock = require_same_clock(&[a.clone(), b.clone()])?;
    let id = hash_id(tag, &[a.id(), b.id()], |_| {});
    let readers = vec![a.subscribe(), b.subscribe()];
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        Some(clock),
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _a = a;
            let _b = b;
            run_aligned_emit(readers, output, move |xs, out| {
                let v = f(xs[0], xs[1]);
                out.extend_from_slice(&v.to_le_bytes());
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

/// Element-wise mean of N co-clocked f64 inputs.
pub fn mean(inputs: Vec<Arc<dyn DynamicNode>>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    if inputs.is_empty() {
        return Err(BuildError::EmptyInputs);
    }
    let schema = require_f64_scalar(&inputs[0])?;
    for node in &inputs[1..] {
        let _ = require_f64_scalar(node)?;
    }
    let clock = require_same_clock(&inputs)?;
    let parent_ids: Vec<NodeId> = inputs.iter().map(|n| n.id()).collect();
    let id = hash_id(op_tag::MEAN, &parent_ids, |_| {});
    let n = inputs.len() as f64;
    let readers: Vec<NodeReader> = inputs.iter().map(|n| n.subscribe()).collect();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        Some(clock),
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _inputs = inputs;
            run_aligned_emit(readers, output, move |xs, out| {
                let v = xs.iter().sum::<f64>() / n;
                out.extend_from_slice(&v.to_le_bytes());
            })
            .await;
        },
    ))
}

/// Pack N co-clocked f64 scalars into a single `F64[N]` vector component.
/// Inputs must share a clock; output schema is `[N]`-shaped, so downstream
/// ops that expect a scalar will reject it (use `Persist` or another vec
/// consumer instead).
pub fn pack(inputs: Vec<Arc<dyn DynamicNode>>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    if inputs.is_empty() {
        return Err(BuildError::EmptyInputs);
    }
    for node in &inputs {
        let _ = require_f64_scalar(node)?;
    }
    let clock = require_same_clock(&inputs)?;
    let n = inputs.len();
    let parent_ids: Vec<NodeId> = inputs.iter().map(|n| n.id()).collect();
    let id = hash_id(op_tag::PACK, &parent_ids, |_| {});
    let schema = metor_db::ComponentSchema::new(metor_proto::types::PrimType::F64, &[n]);
    let readers: Vec<NodeReader> = inputs.iter().map(|n| n.subscribe()).collect();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        Some(clock),
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _inputs = inputs;
            run_aligned_emit(readers, output, |xs, out| {
                for x in xs {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            })
            .await;
        },
    ))
}
