//! Resample an input value stream onto a target clock. The strategy is
//! selected by [`ResampleMode`]:
//!
//! - `Zoh` / `LatestAt`: zero-order hold — emit the most recent input bytes
//!   on every tick (dtype/shape passthrough; no decoding needed).
//! - `Linear`: per-element linear interpolation in `f64`. Output dtype = input
//!   dtype (rounded back) for floats; integer inputs are promoted to `f64`.

use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::Arc;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, NodeReader, ValueType, default_ring_bytes,
    hash_id, op_tag, require_clock, require_value,
};
use crate::dynamic::tensor::{is_float, read_f64_at, shape_elems, write_f64_at};
use metor_db::{ComponentSchema, disruptor::Disruptor};
use metor_proto::types::{PrimType, Timestamp};

/// Pop a buffer off the free-list (or allocate a fresh one) and fill it from
/// `src` using `clear` + `extend_from_slice`, reusing capacity.
fn take_byte_slot(free_slots: &mut Vec<Vec<u8>>, src: &[u8]) -> Vec<u8> {
    let mut slot = free_slots.pop().unwrap_or_default();
    slot.clear();
    slot.extend_from_slice(src);
    slot
}

/// Pop a Vec<f64> off the free-list (or allocate fresh) and fill it from a
/// row-major byte buffer of `dtype` of length `n_elems`.
fn take_decoded_slot(
    free_slots: &mut Vec<Vec<f64>>,
    src: &[u8],
    dtype: PrimType,
    n_elems: usize,
) -> Vec<f64> {
    let mut slot = free_slots.pop().unwrap_or_default();
    slot.clear();
    slot.reserve(n_elems);
    for i in 0..n_elems {
        slot.push(read_f64_at(src, dtype, i));
    }
    slot
}

/// Zoh / LatestAt: byte passthrough.
async fn run_resample_zoh(
    mut input_reader: NodeReader,
    mut clock_reader: NodeReader,
    output: Disruptor,
    in_value_size: usize,
) {
    let out_msg_size = size_of::<Timestamp>() + in_value_size;
    let mut prev: Option<(Timestamp, Vec<u8>)> = None;
    let mut pending: VecDeque<(Timestamp, Vec<u8>)> = VecDeque::new();
    let mut free_slots: Vec<Vec<u8>> = Vec::new();
    fn drain_input(
        reader: &mut NodeReader,
        pending: &mut VecDeque<(Timestamp, Vec<u8>)>,
        free_slots: &mut Vec<Vec<u8>>,
        expected: usize,
    ) {
        while let Some(grant) = reader.try_next() {
            for (ts, v) in grant.samples() {
                if v.len() == expected {
                    pending.push_back((ts, take_byte_slot(free_slots, v)));
                }
            }
        }
    }
    loop {
        drain_input(
            &mut input_reader,
            &mut pending,
            &mut free_slots,
            in_value_size,
        );
        let clock_grant = clock_reader.next().await;
        for (tick, _) in clock_grant.samples() {
            drain_input(
                &mut input_reader,
                &mut pending,
                &mut free_slots,
                in_value_size,
            );
            // Advance prev while the front is <= tick; recycle the previous prev's buffer.
            while let Some(front) = pending.front()
                && front.0.0 <= tick.0
            {
                if let Some((_, old)) = prev.take() {
                    free_slots.push(old);
                }
                prev = pending.pop_front();
            }
            // Choose value: prev if any, else front (oldest pending).
            let value = prev
                .as_ref()
                .or_else(|| pending.front())
                .map(|(_, v)| v.as_slice());
            let Some(v) = value else {
                continue;
            };
            let Ok(mut wg) = output.try_grant(out_msg_size) else {
                continue;
            };
            wg[..size_of::<Timestamp>()].copy_from_slice(&tick.to_le_bytes());
            wg[size_of::<Timestamp>()..].copy_from_slice(v);
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
    let out_msg_size = size_of::<Timestamp>() + out_dtype.size() * n_elems;
    // Decoded f64 buffers per sample so we don't redecode prev/next per tick.
    let mut prev: Option<(Timestamp, Vec<f64>)> = None;
    let mut pending: VecDeque<(Timestamp, Vec<f64>)> = VecDeque::new();
    let mut free_slots: Vec<Vec<f64>> = Vec::new();
    fn drain_input(
        reader: &mut NodeReader,
        pending: &mut VecDeque<(Timestamp, Vec<f64>)>,
        free_slots: &mut Vec<Vec<f64>>,
        expected: usize,
        dtype: PrimType,
        n_elems: usize,
    ) {
        while let Some(grant) = reader.try_next() {
            for (ts, v) in grant.samples() {
                if v.len() != expected {
                    continue;
                }
                pending.push_back((ts, take_decoded_slot(free_slots, v, dtype, n_elems)));
            }
        }
    }
    loop {
        drain_input(
            &mut input_reader,
            &mut pending,
            &mut free_slots,
            in_value_size,
            in_dtype,
            n_elems,
        );
        let clock_grant = clock_reader.next().await;
        for (tick, _) in clock_grant.samples() {
            drain_input(
                &mut input_reader,
                &mut pending,
                &mut free_slots,
                in_value_size,
                in_dtype,
                n_elems,
            );
            while let Some(front) = pending.front()
                && front.0.0 <= tick.0
            {
                if let Some((_, old)) = prev.take() {
                    free_slots.push(old);
                }
                prev = pending.pop_front();
            }
            let next = pending.front();
            // Bail before granting if we have nothing to emit.
            if prev.is_none() && next.is_none() {
                continue;
            }
            let Ok(mut wg) = output.try_grant(out_msg_size) else {
                continue;
            };
            wg[..size_of::<Timestamp>()].copy_from_slice(&tick.to_le_bytes());
            let out_bytes = &mut wg[size_of::<Timestamp>()..];
            match (&prev, next) {
                (None, None) => unreachable!(),
                (None, Some((_, v))) | (Some((_, v)), None) => {
                    for (i, &x) in v.iter().enumerate() {
                        write_f64_at(out_bytes, out_dtype, i, x);
                    }
                }
                (Some((t0, v0)), Some((t1, v1))) => {
                    if tick.0 <= t0.0 {
                        for (i, &x) in v0.iter().enumerate() {
                            write_f64_at(out_bytes, out_dtype, i, x);
                        }
                    } else if tick.0 >= t1.0 || t1.0 == t0.0 {
                        for (i, &x) in v1.iter().enumerate() {
                            write_f64_at(out_bytes, out_dtype, i, x);
                        }
                    } else {
                        let frac = (tick.0 - t0.0) as f64 / (t1.0 - t0.0) as f64;
                        for (i, (a, b)) in v0.iter().zip(v1.iter()).enumerate() {
                            write_f64_at(out_bytes, out_dtype, i, a + frac * (b - a));
                        }
                    }
                }
            }
        }
    }
}

/// Resampling strategy. `Zoh` and `LatestAt` share semantics (zero-order
/// hold) but get distinct `op_tag`s so the editor can label them differently
/// and existing serialized graphs reconcile to the same `NodeId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, facet::Facet)]
#[repr(u8)]
pub enum ResampleMode {
    /// Zero-order hold: emit the most recent input bytes on every tick.
    Zoh,
    /// Per-element linear interpolation in `f64`.
    Linear,
    /// Same semantics as `Zoh`; distinct tag for UI labeling.
    LatestAt,
}

impl ResampleMode {
    pub fn op_tag(self) -> &'static [u8] {
        match self {
            ResampleMode::Zoh => op_tag::ZOH,
            ResampleMode::Linear => op_tag::LINEAR,
            ResampleMode::LatestAt => op_tag::LATEST_AT,
        }
    }
}

pub fn resample(
    input: Arc<dyn DynamicNode>,
    clock: Arc<dyn DynamicNode>,
    mode: ResampleMode,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_value(&input)?;
    require_clock(&clock)?;
    let parent_clock = clock.parent_clock_id();
    let id = hash_id(mode.op_tag(), &[input.id(), clock.id()], |_| {});
    let input_reader = input.subscribe();
    let clock_reader = clock.subscribe();
    let in_value_size = in_schema.size();
    match mode {
        ResampleMode::Zoh | ResampleMode::LatestAt => {
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
        ResampleMode::Linear => {
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
