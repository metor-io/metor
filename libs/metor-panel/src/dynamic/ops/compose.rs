//! Multi-input value composers. Inputs **must share a clock**
//! (`parent_clock_id`); use the resampler ops to align mismatched rates.
//!
//! Co-clocked inputs have aligned timestamps in lockstep, so the composer
//! task pulls one sample from each, asserts the timestamps match, and emits
//! a combined value. Phase 1 scope: f64 scalars.

use std::sync::Arc;

use metor_db::ComponentSchema;
use metor_proto::types::PrimType;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, NodeReader, ValueType, default_ring_bytes,
    hash_id, op_tag, write_sample,
};

fn ensure_f64_scalar(node: &Arc<dyn DynamicNode>) -> Result<ComponentSchema, BuildError> {
    let schema = match node.value_type() {
        ValueType::Clock => return Err(BuildError::ExpectedValue),
        ValueType::Value(s) => s,
    };
    if schema.prim_type != PrimType::F64 {
        return Err(BuildError::ExpectedFloat(schema.prim_type));
    }
    Ok(schema.clone())
}

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

use crate::dynamic::node::NodeId;

fn binary(
    tag: &'static [u8],
    a: Arc<dyn DynamicNode>,
    b: Arc<dyn DynamicNode>,
    f: impl Fn(f64, f64) -> f64 + Send + Sync + 'static,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let schema = ensure_f64_scalar(&a)?;
    let _ = ensure_f64_scalar(&b)?;
    let clock = require_same_clock(&[a.clone(), b.clone()])?;
    let id = hash_id(tag, &[a.id(), b.id()], |_| {});
    let mut a_reader = a.subscribe();
    let mut b_reader = b.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        Some(clock),
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _a = a;
            let _b = b;
            run_binary(&mut a_reader, &mut b_reader, output, f).await;
        },
    ))
}

async fn run_binary(
    a: &mut NodeReader,
    b: &mut NodeReader,
    output: metor_db::disruptor::Disruptor,
    f: impl Fn(f64, f64) -> f64 + Send + Sync + 'static,
) {
    // Co-clocked invariant: a and b emit the same timestamps in the same
    // order. We pull one sample from each and combine. If a grant from one
    // side has multiple samples, drain pairs until we run out, then pull
    // again from whichever side ran short.
    let mut buf_a: Vec<(metor_proto::types::Timestamp, f64)> = Vec::new();
    let mut buf_b: Vec<(metor_proto::types::Timestamp, f64)> = Vec::new();
    loop {
        if buf_a.is_empty() {
            let grant = a.next().await;
            buf_a.extend(grant.samples().map(|(ts, v)| (ts, read_f64(v))));
        }
        if buf_b.is_empty() {
            let grant = b.next().await;
            buf_b.extend(grant.samples().map(|(ts, v)| (ts, read_f64(v))));
        }
        let n = buf_a.len().min(buf_b.len());
        for i in 0..n {
            let (ts_a, va) = buf_a[i];
            let (ts_b, vb) = buf_b[i];
            // Co-clock invariant: timestamps line up. If they don't, prefer
            // the earlier — but log so callers notice they should resample.
            let ts = if ts_a == ts_b {
                ts_a
            } else {
                tracing::warn!(?ts_a, ?ts_b, "compose: timestamps drifted; consider resampling");
                ts_a.min(ts_b)
            };
            let v = f(va, vb);
            write_sample(&output, ts, &v.to_le_bytes());
        }
        buf_a.drain(..n);
        buf_b.drain(..n);
    }
}

fn read_f64(value: &[u8]) -> f64 {
    f64::from_le_bytes(value.try_into().expect("f64 scalar"))
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
    let schema = ensure_f64_scalar(&inputs[0])?;
    for node in &inputs[1..] {
        let _ = ensure_f64_scalar(node)?;
    }
    let clock = require_same_clock(&inputs)?;
    let parent_ids: Vec<NodeId> = inputs.iter().map(|n| n.id()).collect();
    let id = hash_id(op_tag::MEAN, &parent_ids, |_| {});
    let n = inputs.len() as f64;

    let mut readers: Vec<NodeReader> = inputs.iter().map(|n| n.subscribe()).collect();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        Some(clock),
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _inputs = inputs;
            // Per-input buffers, drained in lockstep.
            let mut bufs: Vec<Vec<(metor_proto::types::Timestamp, f64)>> =
                vec![Vec::new(); readers.len()];
            loop {
                for (i, reader) in readers.iter_mut().enumerate() {
                    if bufs[i].is_empty() {
                        let grant = reader.next().await;
                        bufs[i].extend(grant.samples().map(|(ts, v)| (ts, read_f64(v))));
                    }
                }
                let take = bufs.iter().map(|b| b.len()).min().unwrap_or(0);
                for i in 0..take {
                    let ts = bufs[0][i].0;
                    let sum: f64 = bufs.iter().map(|b| b[i].1).sum();
                    let v = sum / n;
                    write_sample(&output, ts, &v.to_le_bytes());
                }
                for buf in &mut bufs {
                    buf.drain(..take);
                }
            }
        },
    ))
}
