//! Single-input value-to-value derivations.
//!
//! Phase 1 scope: f64 scalar in, f64 scalar out. Extending to other prim
//! types or vector shapes is a straight repeat of the same pattern (read
//! input, transform, write output) — defer until needed.

use std::hash::Hash;
use std::sync::Arc;

use metor_db::ComponentSchema;
use metor_proto::types::PrimType;

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
