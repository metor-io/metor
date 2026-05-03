//! Single-input value-to-value derivations.
//!
//! Phase 1 scope: f64 scalar in, f64 scalar out. Extending to other prim
//! types or vector shapes is a straight repeat of the same pattern (read
//! input, transform, write output) — defer until needed.

use std::collections::VecDeque;
use std::hash::Hash;
use std::sync::Arc;

use metor_db::ComponentSchema;
use metor_proto::types::PrimType;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, ValueType, default_ring_bytes, hash_id,
    op_tag, require_f64_scalar, write_sample,
};

/// `require_f64_scalar` plus the single-element-shape allowance derive ops
/// accept (so a `Vec3<f64>` element pulled out as a scalar still works).
fn require_f64_derivable(node: &Arc<dyn DynamicNode>) -> Result<ComponentSchema, BuildError> {
    let schema = require_f64_scalar(node)?;
    if !schema.dim.is_empty() && schema.dim.iter().product::<usize>() != 1 {
        return Err(BuildError::SchemaMismatch {
            a: schema.clone(),
            b: ComponentSchema::new(PrimType::F64, &[]),
        });
    }
    Ok(schema)
}

fn read_f64(value: &[u8]) -> f64 {
    f64::from_le_bytes(value.try_into().expect("f64 scalar"))
}

fn map(
    tag: &'static [u8],
    input: Arc<dyn DynamicNode>,
    extra_args: impl FnOnce(&mut std::collections::hash_map::DefaultHasher),
    f: impl Fn(f64) -> f64 + Send + Sync + 'static,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let schema = require_f64_derivable(&input)?;
    let id = hash_id(tag, &[input.id()], extra_args);
    let parent_clock = input.parent_clock_id();
    let mut reader = input.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _input = input;
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    let v = f(read_f64(value));
                    write_sample(&output, ts, &v.to_le_bytes());
                }
            }
        },
    ))
}

pub fn scale(input: Arc<dyn DynamicNode>, k: f64) -> Result<Arc<dyn DynamicNode>, BuildError> {
    map(
        op_tag::SCALE,
        input,
        |h| {
            k.to_bits().hash(h);
        },
        move |x| x * k,
    )
}

pub fn offset(input: Arc<dyn DynamicNode>, k: f64) -> Result<Arc<dyn DynamicNode>, BuildError> {
    map(
        op_tag::OFFSET,
        input,
        |h| {
            k.to_bits().hash(h);
        },
        move |x| x + k,
    )
}

pub fn abs(input: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    map(op_tag::ABS, input, |_| {}, f64::abs)
}

pub fn neg(input: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    map(op_tag::NEG, input, |_| {}, |x| -x)
}

pub fn log(input: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    map(op_tag::LOG, input, |_| {}, f64::ln)
}

/// One-sided magnitude FFT of a real-valued vector input.
///
/// Input must be `Value(F64, [..])` whose total element count is `N >= 2`.
/// Output is `[f64; N/2 + 1]` — the magnitudes of bins 0..=N/2 from a
/// forward FFT (`Complex` form internally; the real input fills the real
/// component, imag stays zero).
///
/// Per-tick allocation is bounded: the complex scratch buffer and an
/// output byte scratch are both reused. The `Arc<dyn Fft<f64>>` returned
/// by `FftPlanner` is `Send + Sync`, so it lives in the spawn closure.
pub fn fft(input: Arc<dyn DynamicNode>) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let in_schema = require_f64_scalar(&input)?;
    let n: usize = in_schema.dim.iter().product::<usize>().max(1);
    if n < 2 {
        return Err(BuildError::InvalidArg {
            op: "fft",
            reason: "input must be a vector of length >= 2",
        });
    }
    let out_len = n / 2 + 1;
    let id = hash_id(op_tag::FFT, &[input.id()], |_| {});
    let parent_clock = input.parent_clock_id();
    let out_schema = ComponentSchema::new(PrimType::F64, &[out_len]);
    let mut reader = input.subscribe();
    let mut planner = FftPlanner::<f64>::new();
    let plan = planner.plan_fft_forward(n);
    let in_size = n * size_of::<f64>();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(out_schema.clone()),
        parent_clock,
        default_ring_bytes(out_schema.size()),
        move |output| async move {
            let _input = input;
            let mut buf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); n];
            let mut scratch: Vec<u8> = Vec::with_capacity(out_len * size_of::<f64>());
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    if value.len() != in_size {
                        continue;
                    }
                    for (i, chunk) in value.chunks_exact(8).enumerate() {
                        let v = f64::from_le_bytes(chunk.try_into().expect("8-byte chunk"));
                        buf[i] = Complex::new(v, 0.0);
                    }
                    plan.process(&mut buf);
                    scratch.clear();
                    for c in buf.iter().take(out_len) {
                        scratch.extend_from_slice(&c.norm().to_le_bytes());
                    }
                    write_sample(&output, ts, &scratch);
                }
            }
        },
    ))
}

/// Sliding window over a scalar f64 stream. Emits an `[f64; size]` vector
/// on every input tick: the buffer is preloaded with zeros, so emissions
/// start immediately rather than waiting for `size` samples to accumulate.
/// The leading zeros wash out after `size` ticks.
pub fn window(
    input: Arc<dyn DynamicNode>,
    size: usize,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    if size == 0 {
        return Err(BuildError::InvalidArg {
            op: "window",
            reason: "size must be > 0",
        });
    }
    let _ = require_f64_derivable(&input)?;
    let id = hash_id(op_tag::WINDOW, &[input.id()], |h| size.hash(h));
    let parent_clock = input.parent_clock_id();
    let schema = ComponentSchema::new(PrimType::F64, &[size]);
    let mut reader = input.subscribe();
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema.clone()),
        parent_clock,
        default_ring_bytes(schema.size()),
        move |output| async move {
            let _input = input;
            let mut buf: VecDeque<f64> = VecDeque::from(vec![0.0; size]);
            let mut scratch: Vec<u8> = Vec::with_capacity(size * size_of::<f64>());
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    let v = read_f64(value);
                    buf.pop_front();
                    buf.push_back(v);
                    scratch.clear();
                    for x in buf.iter() {
                        scratch.extend_from_slice(&x.to_le_bytes());
                    }
                    write_sample(&output, ts, &scratch);
                }
            }
        },
    ))
}
