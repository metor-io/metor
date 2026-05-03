//! Resample an input value stream onto a target clock.
//!
//! - `zoh` / `latest_at`: zero-order hold — emit the most recent input bytes
//!   on every tick (dtype/shape passthrough; no decoding needed).
//! - `linear`: per-element linear interpolation in `f64`. Output dtype = input
//!   dtype (rounded back) for floats; integer inputs are promoted to `f64`.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, NodeReader, ValueType, default_ring_bytes,
    hash_id, op_tag, require_clock, require_value, write_sample,
};
use crate::dynamic::tensor::{is_float, read_f64_at, shape_elems, write_f64_as};
use metor_db::{ComponentSchema, disruptor::Disruptor};
use metor_proto::types::{PrimType, Timestamp};

/// ZOH/latest-at: byte passthrough.
async fn run_resample_zoh(
    mut input_reader: NodeReader,
    mut clock_reader: NodeReader,
    output: Disruptor,
    in_value_size: usize,
) {
    let mut prev: Option<(Timestamp, Vec<u8>)> = None;
    let mut pending: VecDeque<(Timestamp, Vec<u8>)> = VecDeque::new();
    fn drain_input(reader: &mut NodeReader, pending: &mut VecDeque<(Timestamp, Vec<u8>)>, expected: usize) {
        while let Some(grant) = reader.try_next() {
            for (ts, v) in grant.samples() {
                if v.len() == expected {
                    pending.push_back((ts, v.to_vec()));
                }
            }
        }
    }
    loop {
        drain_input(&mut input_reader, &mut pending, in_value_size);
        let clock_grant = clock_reader.next().await;
        for (tick, _) in clock_grant.samples() {
            drain_input(&mut input_reader, &mut pending, in_value_size);
            // Advance prev while the front is <= tick.
            while let Some(front) = pending.front()
                && front.0.0 <= tick.0
            {
                prev = pending.pop_front();
            }
            // Choose value: prev if any, else front (oldest pending).
            let value = prev.as_ref().or_else(|| pending.front()).map(|(_, v)| v.as_slice());
            if let Some(v) = value {
                write_sample(&output, tick, v);
            }
        }
    }
}

/// Linear: per-element f64 interp.
async fn run_resample_linear(
    mut input_reader: NodeReader,
    mut clock_reader: NodeReader,
    output: Disruptor,
    in_value_size: usize,
    in_dtype: PrimType,
    out_dtype: PrimType,
    n_elems: usize,
) {
    // Decoded f64 buffers per sample so we don't redecode prev/next per tick.
    let mut prev: Option<(Timestamp, Vec<f64>)> = None;
    let mut pending: VecDeque<(Timestamp, Vec<f64>)> = VecDeque::new();
    fn drain_input(
        reader: &mut NodeReader,
        pending: &mut VecDeque<(Timestamp, Vec<f64>)>,
        expected: usize,
        dtype: PrimType,
        n_elems: usize,
    ) {
        while let Some(grant) = reader.try_next() {
            for (ts, v) in grant.samples() {
                if v.len() != expected {
                    continue;
                }
                let mut decoded = Vec::with_capacity(n_elems);
                for i in 0..n_elems {
                    decoded.push(read_f64_at(v, dtype, i));
                }
                pending.push_back((ts, decoded));
            }
        }
    }
    let mut scratch: Vec<u8> = Vec::with_capacity(out_dtype.size() * n_elems);
    loop {
        drain_input(&mut input_reader, &mut pending, in_value_size, in_dtype, n_elems);
        let clock_grant = clock_reader.next().await;
        for (tick, _) in clock_grant.samples() {
            drain_input(&mut input_reader, &mut pending, in_value_size, in_dtype, n_elems);
            while let Some(front) = pending.front()
                && front.0.0 <= tick.0
            {
                prev = pending.pop_front();
            }
            let next = pending.front();
            let interp = match (&prev, next) {
                (None, None) => None,
                (None, Some((_, v))) | (Some((_, v)), None) => Some(v.clone()),
                (Some((t0, v0)), Some((t1, v1))) => {
                    if tick.0 <= t0.0 {
                        Some(v0.clone())
                    } else if tick.0 >= t1.0 {
                        Some(v1.clone())
                    } else {
                        let dt = (t1.0 - t0.0) as f64;
                        if dt == 0.0 {
                            Some(v1.clone())
                        } else {
                            let frac = (tick.0 - t0.0) as f64 / dt;
                            Some(
                                v0.iter()
                                    .zip(v1.iter())
                                    .map(|(a, b)| a + frac * (b - a))
                                    .collect(),
                            )
                        }
                    }
                }
            };
            let Some(values) = interp else { continue };
            scratch.clear();
            for v in &values {
                write_f64_as(&mut scratch, out_dtype, *v);
            }
            write_sample(&output, tick, &scratch);
        }
    }
}

#[derive(Clone, Copy)]
enum Interp {
    Zoh,
    Linear,
}

fn build(
    tag: &'static [u8],
    input: Arc<dyn DynamicNode>,
    clock: Arc<dyn DynamicNode>,
    interp: Interp,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    require_clock(&clock)?;
    let parent_clock = clock.parent_clock_id();
    let id = hash_id(tag, &[input.id(), clock.id()], |_| {});
    let input_reader = input.subscribe();
    let clock_reader = clock.subscribe();
    let in_value_size = in_schema.size();
    match interp {
        Interp::Zoh => {
            let out_schema = in_schema.clone();
            Ok(NodeImpl::spawn(
                id,
                ValueType::Value(out_schema.clone()),
                parent_clock,
                default_ring_bytes(out_schema.size()),
                move |output| async move {
                    let _input = input;
                    let _clock = clock;
                    run_resample_zoh(input_reader, clock_reader, output, in_value_size).await;
                },
            ))
        }
        Interp::Linear => {
            // Promote ints to f64; preserve float widths.
            let out_dtype = if is_float(in_schema.prim_type) {
                in_schema.prim_type
            } else {
                PrimType::F64
            };
            let out_schema = ComponentSchema::new(out_dtype, &in_schema.dim);
            let n_elems = shape_elems(&in_schema.dim);
            let in_dtype = in_schema.prim_type;
            Ok(NodeImpl::spawn(
                id,
                ValueType::Value(out_schema.clone()),
                parent_clock,
                default_ring_bytes(out_schema.size()),
                move |output| async move {
                    let _input = input;
                    let _clock = clock;
                    run_resample_linear(
                        input_reader,
                        clock_reader,
                        output,
                        in_value_size,
                        in_dtype,
                        out_dtype,
                        n_elems,
                    )
                    .await;
                },
            ))
        }
    }
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
