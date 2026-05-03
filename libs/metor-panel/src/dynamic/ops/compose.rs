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
    // order. If a producer drops a sample on a full ring (write_sample's
    // drop-on-full policy), the buffers desync and stay desynced unless we
    // realign. So whenever the heads don't match, we drop the older head
    // and re-check — emitting `f(va, vb)` from misaligned positions would
    // silently combine values from different ticks.
    let mut buf_a: std::collections::VecDeque<(metor_proto::types::Timestamp, f64)> =
        std::collections::VecDeque::new();
    let mut buf_b: std::collections::VecDeque<(metor_proto::types::Timestamp, f64)> =
        std::collections::VecDeque::new();
    loop {
        if buf_a.is_empty() {
            let grant = a.next().await;
            buf_a.extend(grant.samples().map(|(ts, v)| (ts, read_f64(v))));
        }
        if buf_b.is_empty() {
            let grant = b.next().await;
            buf_b.extend(grant.samples().map(|(ts, v)| (ts, read_f64(v))));
        }
        // Realign: drop older head(s) until ts_a == ts_b (or one side empties).
        loop {
            let (Some(&(ts_a, _)), Some(&(ts_b, _))) = (buf_a.front(), buf_b.front()) else {
                break;
            };
            if ts_a == ts_b {
                break;
            }
            tracing::warn!(?ts_a, ?ts_b, "compose: timestamps drifted; dropping older sample");
            if ts_a.0 < ts_b.0 {
                buf_a.pop_front();
            } else {
                buf_b.pop_front();
            }
        }
        // Emit aligned pairs.
        while let (Some(&(ts_a, va)), Some(&(ts_b, vb))) = (buf_a.front(), buf_b.front()) {
            if ts_a != ts_b {
                break;
            }
            buf_a.pop_front();
            buf_b.pop_front();
            let v = f(va, vb);
            write_sample(&output, ts_a, &v.to_le_bytes());
            // The borrow ends with pop_front; bind for clarity.
            let _ = ts_b;
        }
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
            let mut bufs: Vec<std::collections::VecDeque<(metor_proto::types::Timestamp, f64)>> =
                (0..readers.len()).map(|_| std::collections::VecDeque::new()).collect();
            loop {
                for (i, reader) in readers.iter_mut().enumerate() {
                    if bufs[i].is_empty() {
                        let grant = reader.next().await;
                        bufs[i].extend(grant.samples().map(|(ts, v)| (ts, read_f64(v))));
                    }
                }
                // Realign: while heads aren't all equal, drop the oldest head(s).
                // If a producer drops a sample on a full ring, the buffers
                // desync and stay desynced unless we realign — emitting the
                // mean of misaligned samples silently averages different ticks.
                loop {
                    let mut max_ts: Option<metor_proto::types::Timestamp> = None;
                    let mut min_ts: Option<metor_proto::types::Timestamp> = None;
                    let mut any_empty = false;
                    for buf in &bufs {
                        match buf.front() {
                            None => {
                                any_empty = true;
                                break;
                            }
                            Some(&(ts, _)) => {
                                max_ts = Some(match max_ts {
                                    None => ts,
                                    Some(m) if ts.0 > m.0 => ts,
                                    Some(m) => m,
                                });
                                min_ts = Some(match min_ts {
                                    None => ts,
                                    Some(m) if ts.0 < m.0 => ts,
                                    Some(m) => m,
                                });
                            }
                        }
                    }
                    if any_empty {
                        break;
                    }
                    let (Some(max_ts), Some(min_ts)) = (max_ts, min_ts) else {
                        break;
                    };
                    if max_ts == min_ts {
                        break;
                    }
                    tracing::warn!(?min_ts, ?max_ts, "mean: timestamps drifted; dropping older samples");
                    for buf in &mut bufs {
                        if let Some(&(ts, _)) = buf.front()
                            && ts.0 < max_ts.0
                        {
                            buf.pop_front();
                        }
                    }
                }
                // Emit aligned tuples while every buffer's head ts matches.
                loop {
                    let Some((ts0, _)) = bufs[0].front().copied() else {
                        break;
                    };
                    if !bufs.iter().all(|b| b.front().map(|&(ts, _)| ts) == Some(ts0)) {
                        break;
                    }
                    let sum: f64 = bufs
                        .iter_mut()
                        .map(|b| b.pop_front().expect("checked").1)
                        .sum();
                    let v = sum / n;
                    write_sample(&output, ts0, &v.to_le_bytes());
                }
            }
        },
    ))
}
