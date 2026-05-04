//! Dynamic, user-defined components.
//!
//! Each [`DynamicNode`] is a producer task that writes framed
//! `[Timestamp][value bytes]` samples into its own [`Disruptor`]. Subscribers
//! either iterate every sample (via [`NodeReader`], used by derivation tasks)
//! or pull only the latest in each grant (via
//! [`crate::WalComponentStream::from_disruptor`], used by views).
//!
//! Construction goes through one of the `ops` modules. Each constructor
//! returns `Arc<dyn DynamicNode>`; dropping the last `Arc` cancels the
//! producer task. Identity is a content hash so the same `(op, args, parents)`
//! always yields the same `NodeId` — what the future node-editor uses for
//! reconciliation.

mod node;
pub mod ops;
mod registry;
pub mod tensor;

#[cfg(test)]
mod tests;

pub use node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeGrant, NodeId, NodeImpl, NodeReader, ValueType,
    default_ring_bytes, hash_id, op_tag, require_clock, require_value, write_sample,
};
pub use registry::DynamicRegistry;
pub use tensor::TypedScalar;

use std::sync::Arc;

use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::{ComponentStreamBuilder, WalComponentStream};

impl ComponentStreamBuilder for Arc<dyn DynamicNode> {
    type Stream = WalComponentStream;

    fn component_id(&self) -> ComponentId {
        self.id().into()
    }

    async fn into_stream(self, _db: &DB) -> WalComponentStream {
        let schema = self
            .value_type()
            .schema()
            .cloned()
            .expect("clock-typed nodes cannot be rendered as values");
        WalComponentStream::from_disruptor(self.output(), schema)
    }
}
