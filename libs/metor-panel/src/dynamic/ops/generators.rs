//! Generators: nodes that consume a clock and emit one f64 value per tick.

use std::hash::Hash;
use std::sync::Arc;

use metor_db::ComponentSchema;
use metor_proto::types::PrimType;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, ValueType, default_ring_bytes, hash_id,
    op_tag, require_clock, write_sample,
};

fn f64_scalar_schema() -> ComponentSchema {
    ComponentSchema::new(PrimType::F64, &[])
}

/// `sin(2π·freq·t + phase) · amplitude`, sampled at every clock tick.
/// `t` is the clock's wall-clock timestamp in seconds since the unix epoch.
pub fn sin(
    clock: Arc<dyn DynamicNode>,
    freq: f64,
    amplitude: f64,
    phase: f64,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    require_clock(&clock)?;
    let id = hash_id(op_tag::SIN, &[clock.id()], |h| {
        freq.to_bits().hash(h);
        amplitude.to_bits().hash(h);
        phase.to_bits().hash(h);
    });
    let schema = f64_scalar_schema();
    let value_bytes = schema.size();
    let parent_clock = clock.parent_clock_id();
    let mut reader = clock.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema),
        parent_clock,
        default_ring_bytes(value_bytes),
        move |output| async move {
            let _clock = clock;
            let two_pi = std::f64::consts::TAU;
            loop {
                let grant = reader.next().await;
                for (ts, _) in grant.samples() {
                    let t = (ts.0 as f64) * 1e-6;
                    let v = amplitude * f64::sin(two_pi * freq * t + phase);
                    write_sample(&output, ts, &v.to_le_bytes());
                }
            }
        },
    ))
}

/// `square(2π·freq·t + phase) · amplitude`. ±amplitude.
pub fn square(
    clock: Arc<dyn DynamicNode>,
    freq: f64,
    amplitude: f64,
    phase: f64,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    require_clock(&clock)?;
    let id = hash_id(op_tag::SQUARE, &[clock.id()], |h| {
        freq.to_bits().hash(h);
        amplitude.to_bits().hash(h);
        phase.to_bits().hash(h);
    });
    let schema = f64_scalar_schema();
    let parent_clock = clock.parent_clock_id();
    let mut reader = clock.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _clock = clock;
            let two_pi = std::f64::consts::TAU;
            loop {
                let grant = reader.next().await;
                for (ts, _) in grant.samples() {
                    let t = (ts.0 as f64) * 1e-6;
                    let theta = two_pi * freq * t + phase;
                    let s = if theta.sin() >= 0.0 { amplitude } else { -amplitude };
                    write_sample(&output, ts, &s.to_le_bytes());
                }
            }
        },
    ))
}

/// Pseudo-random uniform in `[0, 1)`. Cheap LCG seeded from `seed` and
/// advanced once per tick — deterministic given the same seed and clock.
pub fn random(clock: Arc<dyn DynamicNode>, seed: u64) -> Result<Arc<dyn DynamicNode>, BuildError> {
    require_clock(&clock)?;
    let id = hash_id(op_tag::RANDOM, &[clock.id()], |h| {
        seed.hash(h);
    });
    let schema = f64_scalar_schema();
    let parent_clock = clock.parent_clock_id();
    let mut reader = clock.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _clock = clock;
            let mut state: u64 = seed.max(1);
            loop {
                let grant = reader.next().await;
                for (ts, _) in grant.samples() {
                    // splitmix64
                    state = state.wrapping_add(0x9E3779B97F4A7C15);
                    let mut z = state;
                    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
                    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
                    z ^= z >> 31;
                    let v = (z >> 11) as f64 / (1u64 << 53) as f64;
                    write_sample(&output, ts, &v.to_le_bytes());
                }
            }
        },
    ))
}

/// Always emit `value` on every tick. Useful for testing and as a constant
/// input to composers.
pub fn constant(clock: Arc<dyn DynamicNode>, value: f64) -> Result<Arc<dyn DynamicNode>, BuildError> {
    require_clock(&clock)?;
    let id = hash_id(op_tag::CONSTANT, &[clock.id()], |h| {
        value.to_bits().hash(h);
    });
    let schema = f64_scalar_schema();
    let parent_clock = clock.parent_clock_id();
    let mut reader = clock.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _clock = clock;
            let bytes = value.to_le_bytes();
            loop {
                let grant = reader.next().await;
                for (ts, _) in grant.samples() {
                    write_sample(&output, ts, &bytes);
                }
            }
        },
    ))
}
