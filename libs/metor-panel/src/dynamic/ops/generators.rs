//! Generators: nodes that consume a clock and emit one (typed, possibly
//! shaped) value per tick.

use std::hash::Hash;
use std::sync::Arc;

use metor_db::ComponentSchema;
use metor_proto::types::PrimType;
use smallvec::SmallVec;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, ValueType, default_ring_bytes, hash_id,
    op_tag, require_clock, write_sample,
};
use crate::dynamic::tensor::{TypedScalar, shape_elems, write_f64_as};

/// Periodic waveform shape. Phase 1 set covers the four common shapes; add
/// new variants here and extend [`Waveform::sample`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, facet::Facet)]
#[repr(u8)]
pub enum Waveform {
    Sin,
    Cos,
    Square,
    Sawtooth,
}

impl Waveform {
    /// Sample the unit-amplitude waveform at angular position `theta` (in
    /// radians, already including phase offset). Caller scales by amplitude.
    fn sample(self, theta: f64) -> f64 {
        match self {
            Waveform::Sin => theta.sin(),
            Waveform::Cos => theta.cos(),
            Waveform::Square => {
                if theta.sin() >= 0.0 {
                    1.0
                } else {
                    -1.0
                }
            }
            // Linear ramp from -1 to +1 across each period, repeating.
            Waveform::Sawtooth => {
                let two_pi = std::f64::consts::TAU;
                let frac = (theta / two_pi).rem_euclid(1.0);
                2.0 * frac - 1.0
            }
        }
    }
}

fn schema_from(dtype: PrimType, shape: &[usize]) -> ComponentSchema {
    ComponentSchema::new(dtype, shape)
}

/// `shape(2π·freq·t + phase) · amplitude`, sampled at every tick. With a
/// non-empty `shape` the same scalar value fills every element.
pub fn waveform(
    clock: Arc<dyn DynamicNode>,
    shape: Waveform,
    freq: f64,
    amplitude: f64,
    phase: f64,
    dtype: PrimType,
    out_shape: SmallVec<[usize; 4]>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    require_clock(&clock)?;
    let id = hash_id(op_tag::WAVEFORM, &[clock.id()], |h| {
        (shape as u8).hash(h);
        freq.to_bits().hash(h);
        amplitude.to_bits().hash(h);
        phase.to_bits().hash(h);
        (dtype as u8).hash(h);
        for d in &out_shape {
            d.hash(h);
        }
    });
    let schema = schema_from(dtype, &out_shape);
    let parent_clock = clock.parent_clock_id();
    let mut reader = clock.subscribe();
    let n_elems = shape_elems(&out_shape);
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _clock = clock;
            let two_pi = std::f64::consts::TAU;
            let mut scratch: Vec<u8> = Vec::with_capacity(schema.size());
            loop {
                let grant = reader.next().await;
                for (ts, _) in grant.samples() {
                    let t = (ts.0 as f64) * 1e-6;
                    let theta = two_pi * freq * t + phase;
                    let v = amplitude * shape.sample(theta);
                    scratch.clear();
                    for _ in 0..n_elems {
                        write_f64_as(&mut scratch, dtype, v);
                    }
                    write_sample(&output, ts, &scratch);
                }
            }
        },
    ))
}

/// Pseudo-random uniform in `[0, 1)`. Cheap LCG seeded from `seed` and
/// advanced once per emitted element.
pub fn random(
    clock: Arc<dyn DynamicNode>,
    seed: u64,
    dtype: PrimType,
    out_shape: SmallVec<[usize; 4]>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    require_clock(&clock)?;
    let id = hash_id(op_tag::RANDOM, &[clock.id()], |h| {
        seed.hash(h);
        (dtype as u8).hash(h);
        for d in &out_shape {
            d.hash(h);
        }
    });
    let schema = schema_from(dtype, &out_shape);
    let parent_clock = clock.parent_clock_id();
    let mut reader = clock.subscribe();
    let n_elems = shape_elems(&out_shape);
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _clock = clock;
            let mut state: u64 = seed.max(1);
            let mut scratch: Vec<u8> = Vec::with_capacity(schema.size());
            loop {
                let grant = reader.next().await;
                for (ts, _) in grant.samples() {
                    scratch.clear();
                    for _ in 0..n_elems {
                        // splitmix64
                        state = state.wrapping_add(0x9E3779B97F4A7C15);
                        let mut z = state;
                        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
                        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
                        z ^= z >> 31;
                        let v = (z >> 11) as f64 / (1u64 << 53) as f64;
                        write_f64_as(&mut scratch, dtype, v);
                    }
                    write_sample(&output, ts, &scratch);
                }
            }
        },
    ))
}

/// Always emit `value` (cast to `dtype`) on every tick, broadcasted to
/// `out_shape`.
pub fn constant(
    clock: Arc<dyn DynamicNode>,
    value: TypedScalar,
    out_shape: SmallVec<[usize; 4]>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    require_clock(&clock)?;
    let dtype = value.dtype();
    let id = hash_id(op_tag::CONSTANT, &[clock.id()], |h| {
        value.hash(h);
        for d in &out_shape {
            d.hash(h);
        }
    });
    let schema = schema_from(dtype, &out_shape);
    let parent_clock = clock.parent_clock_id();
    let mut reader = clock.subscribe();
    let n_elems = shape_elems(&out_shape);
    let v_f64 = value.as_f64();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _clock = clock;
            let mut bytes: Vec<u8> = Vec::with_capacity(schema.size());
            for _ in 0..n_elems {
                write_f64_as(&mut bytes, dtype, v_f64);
            }
            loop {
                let grant = reader.next().await;
                for (ts, _) in grant.samples() {
                    write_sample(&output, ts, &bytes);
                }
            }
        },
    ))
}
