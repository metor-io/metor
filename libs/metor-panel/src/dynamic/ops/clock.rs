//! Clock-typed nodes. A clock emits `Timestamp`s only — value bytes are zero
//! length. Generators consume a clock and emit one value per tick.

use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use metor_proto::types::Timestamp;

use crate::dynamic::node::{
    DynamicNode, DynamicNodeExt, NodeImpl, ValueType, default_ring_bytes, hash_id, op_tag,
    write_sample,
};

/// Fixed-rate wall-clock generator. Ticks every `1/hz` seconds starting at
/// the moment construction returns.
pub fn fixed_rate(hz: f64) -> Arc<dyn DynamicNode> {
    assert!(hz.is_finite() && hz > 0.0, "fixed_rate needs a positive finite rate");
    let id = hash_id(op_tag::FIXED_RATE_CLOCK, &[], |h| {
        hz.to_bits().hash(h);
    });
    let period = Duration::from_secs_f64(1.0 / hz);
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
    node
}

/// Treat another node's per-sample timestamps as a clock. Useful for binding
/// a generator to whatever rate an upstream component is ticking at.
pub fn clock_of(source: Arc<dyn DynamicNode>) -> Arc<dyn DynamicNode> {
    let id = hash_id(op_tag::CLOCK_OF, &[source.id()], |_| {});
    let mut reader = source.subscribe();
    NodeImpl::spawn(
        id,
        ValueType::Clock,
        Some(id),
        default_ring_bytes(0),
        move |output| async move {
            // hold `source` alive for the lifetime of the task
            let _source = source;
            loop {
                let grant = reader.next().await;
                for (ts, _) in grant.samples() {
                    write_sample(&output, ts, &[]);
                }
            }
        },
    )
}
