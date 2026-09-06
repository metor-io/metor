//! Clock-typed nodes. A clock emits `Timestamp`s only — value bytes are zero
//! length.
//!
//! One clock is left, and it is the one a source system asks for: a
//! `@system(rate=)` declares how often it wants to run and the host answers
//! with a fixed-rate tick.

use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use metor_proto::types::Timestamp;

use crate::dynamic::node::{
    BuildError, DynamicNode, NodeImpl, ValueType, default_ring_bytes, hash_id, op_tag, write_sample,
};

/// Fixed-rate wall-clock generator. Ticks every `1/hz` seconds starting at
/// the moment construction returns.
pub fn fixed_rate(hz: f64) -> Result<Arc<dyn DynamicNode>, BuildError> {
    if !hz.is_finite() || hz <= 0.0 {
        return Err(BuildError::InvalidArg {
            op: "fixed_rate",
            reason: "hz must be finite and positive",
        });
    }
    let period_secs = 1.0 / hz;
    if !period_secs.is_finite() {
        return Err(BuildError::InvalidArg {
            op: "fixed_rate",
            reason: "1/hz overflowed to infinity",
        });
    }
    let id = hash_id(op_tag::FIXED_RATE_CLOCK, &[], |h| {
        hz.to_bits().hash(h);
    });
    let period = Duration::from_secs_f64(period_secs);
    let node = NodeImpl::spawn(
        id,
        ValueType::Clock,
        Some(id), // a clock is its own clock
        default_ring_bytes(0),
        move |output| async move {
            loop {
                let ts = Timestamp::now();
                write_sample(&output, ts, &[]);
                stellarator::sleep(period).await;
            }
        },
    );
    Ok(node)
}
