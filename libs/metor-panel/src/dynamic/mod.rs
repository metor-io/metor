//! Dynamic, user-defined components.
//!
//! Each [`DynamicNode`] is a producer task that writes framed
//! `[Timestamp][value bytes]` samples into its own [`Disruptor`]. Subscribers
//! either iterate every sample (via [`NodeReader`], used by derivation tasks)
//! or read the published component like any other (views).
//!
//! Construction goes through one of the `ops` modules. Each constructor
//! returns `Arc<dyn DynamicNode>`; dropping the last `Arc` cancels the
//! producer task, which is the whole of a teardown — whoever wants a node
//! alive holds it. Identity is a content hash, so the same
//! `(op, args, parents)` always yields the same `NodeId` and an edit rebuilds
//! only what actually changed.

pub mod expressions;
mod node;
pub mod ops;
pub mod resolver;
pub mod tensor;
pub mod worker;

#[cfg(test)]
mod expression_tests;

pub use node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeGrant, NodeId, NodeImpl, NodeReader, ValueType,
    default_ring_bytes, hash_id, op_tag, require_value, write_sample,
};
pub use tensor::TypedScalar;
