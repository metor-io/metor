//! Resample an input value stream onto a target clock.
//!
//! - `zoh`: zero-order hold — emit the most recent input sample at every
//!   tick.
//! - `linear`: linear interpolation between the two surrounding input
//!   samples.
//! - `latest_at`: alias for `zoh` semantically; provided for naming
//!   parallelism with the prompt.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, NodeReader, ValueType, default_ring_bytes,
    hash_id, op_tag, require_clock, require_f64_scalar, write_sample,
};
use metor_db::disruptor::Disruptor;
use metor_proto::types::Timestamp;

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
    // Pending future input samples (ts > prev.ts, not yet consumed by a
    // tick). We walk this in lockstep with the clock so ZOH/Linear get the
    // correct surrounding pair when input runs faster than the output
    // clock — collapsing to a 2-slot window discards intermediate samples
    // and silently picks future-side values for ticks in the past.
    let mut pending: VecDeque<(Timestamp, f64)> = VecDeque::new();
    // Most recent input sample whose ts <= last seen tick.
    let mut prev: Option<(Timestamp, f64)> = None;

    /// Drain everything currently available on the input reader into
    /// `pending`. Non-blocking.
    fn drain_input(reader: &mut NodeReader, pending: &mut VecDeque<(Timestamp, f64)>) {
        while let Some(grant) = reader.try_next() {
            for (ts, v) in grant.samples() {
                pending.push_back((ts, read_f64(v)));
            }
        }
    }

    loop {
        drain_input(&mut input_reader, &mut pending);

        let clock_grant = clock_reader.next().await;
        for (tick, _) in clock_grant.samples() {
            // Re-drain in case more input landed between ticks.
            drain_input(&mut input_reader, &mut pending);

            // Advance `prev` while the queue front is <= tick.
            while let Some(&(ts, _)) = pending.front()
                && ts.0 <= tick.0
            {
                prev = pending.pop_front();
            }
            // `next` is the queue front (the first sample with ts > tick),
            // peeked without removing — a later tick may still need it.
            let next = pending.front().copied();

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
    let schema = require_f64_scalar(&input)?;
    require_clock(&clock)?;
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

/// Same semantics as [`zoh`]; distinct op_tag so the editor can label edges
/// differently.
pub fn latest_at(
    input: Arc<dyn DynamicNode>,
    clock: Arc<dyn DynamicNode>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    build(op_tag::LATEST_AT, input, clock, Interp::Zoh)
}
