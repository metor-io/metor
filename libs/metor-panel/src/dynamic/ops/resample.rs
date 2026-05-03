//! Resample an input value stream onto a target clock.
//!
//! - `zoh`: zero-order hold — emit the most recent input sample at every
//!   tick.
//! - `linear`: linear interpolation between the two surrounding input
//!   samples.
//! - `latest_at`: alias for `zoh` semantically; provided for naming
//!   parallelism with the prompt.

use std::sync::Arc;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, NodeReader, ValueType, default_ring_bytes,
    hash_id, op_tag, write_sample,
};
use metor_db::disruptor::Disruptor;
use metor_db::ComponentSchema;
use metor_proto::types::{PrimType, Timestamp};

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

fn ensure_clock(node: &Arc<dyn DynamicNode>) -> Result<(), BuildError> {
    match node.value_type() {
        ValueType::Clock => Ok(()),
        v => Err(BuildError::ExpectedClock(v.clone())),
    }
}

fn read_f64(b: &[u8]) -> f64 {
    f64::from_le_bytes(b.try_into().expect("f64"))
}

/// Internal task driver shared by `zoh` and `linear`. Pulls clock ticks and
/// input samples concurrently using `try_next` so neither side starves.
async fn run_resample(
    mut input_reader: NodeReader,
    mut clock_reader: NodeReader,
    output: Disruptor,
    interp: Interp,
) {
    // Buffer input samples by timestamp; on each clock tick, emit the
    // resampled value at that timestamp. We keep only the two most recent
    // input samples (enough for ZOH and linear) plus any future ones we
    // haven't reached yet.
    let mut prev: Option<(Timestamp, f64)> = None;
    let mut next: Option<(Timestamp, f64)> = None;

    loop {
        // Try to drain any input first (non-blocking).
        if let Some(grant) = input_reader.try_next_grant() {
            for (ts, v) in grant.samples() {
                let v = read_f64(v);
                if let Some(n) = next.take() {
                    prev = Some(n);
                }
                next = Some((ts, v));
            }
        }

        let clock_grant = clock_reader.next().await;
        for (tick, _) in clock_grant.samples() {
            // Advance prev/next so that prev.ts <= tick < next.ts when
            // possible. Drain any input that's now earlier than `tick`.
            loop {
                if let Some(grant) = input_reader.try_next_grant() {
                    for (ts, v) in grant.samples() {
                        let v = read_f64(v);
                        prev = next.take();
                        next = Some((ts, v));
                    }
                } else {
                    break;
                }
            }
            // No data yet — skip this tick.
            let Some(value) = sample(prev, next, tick, interp) else {
                continue;
            };
            write_sample(&output, tick, &value.to_le_bytes());
        }
    }
}

#[derive(Clone, Copy)]
enum Interp {
    Zoh,
    Linear,
}

fn sample(
    prev: Option<(Timestamp, f64)>,
    next: Option<(Timestamp, f64)>,
    tick: Timestamp,
    interp: Interp,
) -> Option<f64> {
    match (prev, next, interp) {
        // No samples yet.
        (None, None, _) => None,
        // Only one sample — hold it (works for both ZOH and linear).
        (None, Some((_, v)), _) | (Some((_, v)), None, _) => Some(v),
        (Some((t0, v0)), Some((t1, v1)), interp) => {
            if tick.0 <= t0.0 {
                Some(v0)
            } else if tick.0 >= t1.0 {
                Some(v1)
            } else {
                match interp {
                    Interp::Zoh => Some(v0),
                    Interp::Linear => {
                        let dt = (t1.0 - t0.0) as f64;
                        if dt == 0.0 {
                            Some(v1)
                        } else {
                            let frac = (tick.0 - t0.0) as f64 / dt;
                            Some(v0 + frac * (v1 - v0))
                        }
                    }
                }
            }
        }
    }
}

fn build(
    tag: &'static [u8],
    input: Arc<dyn DynamicNode>,
    clock: Arc<dyn DynamicNode>,
    interp: Interp,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let schema = ensure_f64_scalar(&input)?;
    ensure_clock(&clock)?;
    let parent_clock = clock.parent_clock_id();
    let id = hash_id(tag, &[input.id(), clock.id()], |_| {});
    let input_reader = input.subscribe();
    let clock_reader = clock.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _input = input;
            let _clock = clock;
            run_resample(input_reader, clock_reader, output, interp).await;
        },
    ))
}

pub fn zoh(
    input: Arc<dyn DynamicNode>,
    clock: Arc<dyn DynamicNode>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    build(op_tag::ZOH, input, clock, Interp::Zoh)
}

pub fn linear(
    input: Arc<dyn DynamicNode>,
    clock: Arc<dyn DynamicNode>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    build(op_tag::LINEAR, input, clock, Interp::Linear)
}

pub fn latest_at(
    input: Arc<dyn DynamicNode>,
    clock: Arc<dyn DynamicNode>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    // Same semantics as zoh; distinct op_tag so the editor can label edges
    // differently.
    let schema = ensure_f64_scalar(&input)?;
    ensure_clock(&clock)?;
    let parent_clock = clock.parent_clock_id();
    let id = hash_id(op_tag::LATEST_AT, &[input.id(), clock.id()], |_| {});
    let input_reader = input.subscribe();
    let clock_reader = clock.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _input = input;
            let _clock = clock;
            run_resample(input_reader, clock_reader, output, Interp::Zoh).await;
        },
    ))
}
