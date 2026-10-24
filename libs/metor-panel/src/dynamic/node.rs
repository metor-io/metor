use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::sync::Arc;

use metor_db::ComponentSchema;
use metor_db::disruptor::{Disruptor, ReadGrant, Reader};
use metor_proto::types::{ComponentId, Timestamp};
use stellarator::JoinHandleDropGuard;

/// Stable identity of a dynamic node. Hashed from `(op_tag, args, parent_ids)`
/// so re-issuing the same construction returns the same id, and any arg edit
/// produces a fresh one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

impl From<NodeId> for ComponentId {
    fn from(id: NodeId) -> Self {
        ComponentId(id.0)
    }
}

/// A node either carries values (a real component-shaped sample) or just
/// timestamps (a clock).
#[derive(Clone, Debug, PartialEq)]
pub enum ValueType {
    Clock,
    Value(ComponentSchema),
}

impl ValueType {
    pub fn schema(&self) -> Option<&ComponentSchema> {
        match self {
            ValueType::Clock => None,
            ValueType::Value(s) => Some(s),
        }
    }

    /// Per-sample value byte size on the wire (excludes the timestamp).
    pub fn value_bytes(&self) -> usize {
        match self {
            ValueType::Clock => 0,
            ValueType::Value(s) => s.size(),
        }
    }
}

/// Validate that `node` is value-typed (any prim type, any shape) and return
/// a clone of its schema.
pub fn require_value(
    node: &std::sync::Arc<dyn DynamicNode>,
) -> Result<ComponentSchema, BuildError> {
    match node.value_type() {
        ValueType::Clock => Err(BuildError::ExpectedValue),
        ValueType::Value(s) => Ok(s.clone()),
    }
}

/// Errors surfaced when *building* a node — schema mismatches, missing
/// components, or a program that does not fit its inputs.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("expected a value input, got Clock")]
    ExpectedValue,
    #[error("schema mismatch: {a:?} vs {b:?}")]
    SchemaMismatch {
        a: ComponentSchema,
        b: ComponentSchema,
    },
    #[error("{op} expected {expected} input(s), got {got}")]
    WrongArity {
        op: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("a parent node failed to build")]
    ParentFailed,
    #[error("invalid arg for {op}: {reason}")]
    InvalidArg {
        op: &'static str,
        reason: &'static str,
    },
    #[error("component {0:?} not found")]
    ComponentNotFound(ComponentId),
    #[error("db error: {0}")]
    DbError(String),
    #[error("{0}")]
    Expr(String),
}

/// A live producer task whose output is a fan-out [`Disruptor`].
///
/// Drop the last `Arc<dyn DynamicNode>` to cancel the task: the node holds a
/// `JoinHandleDropGuard` which calls `cancel` on drop.
pub trait DynamicNode: Send + Sync + 'static {
    fn id(&self) -> NodeId;
    fn value_type(&self) -> &ValueType;
    /// Output ring. Subscribers read framed `[Timestamp][value bytes]`
    /// messages; clock nodes emit zero value bytes.
    fn output(&self) -> &Disruptor;
}

/// Helpers attached to anything that implements [`DynamicNode`] — kept in a
/// separate ext trait so the trait stays object-safe.
pub trait DynamicNodeExt: DynamicNode {
    fn subscribe(&self) -> NodeReader {
        NodeReader::new(self.output(), self.value_type().value_bytes())
    }
}
impl<T: DynamicNode + ?Sized> DynamicNodeExt for T {}

/// Internal reader handed to downstream program tasks. Yields every sample
/// in order — the opposite of [`crate::WalComponentStream`], which collapses
/// to the latest.
pub struct NodeReader {
    reader: Reader,
    msg_size: usize,
}

impl NodeReader {
    fn new(disruptor: &Disruptor, value_bytes: usize) -> Self {
        Self {
            reader: disruptor.reader(),
            msg_size: size_of::<Timestamp>() + value_bytes,
        }
    }

    pub async fn next(&mut self) -> NodeGrant<'_> {
        let grant = self.reader.next().await;
        NodeGrant {
            grant,
            msg_size: self.msg_size,
        }
    }

    /// Non-blocking grant fetch. Returns `None` if no new data is available.
    pub fn try_next(&mut self) -> Option<NodeGrant<'_>> {
        let grant = self.reader.try_next()?;
        Some(NodeGrant {
            grant,
            msg_size: self.msg_size,
        })
    }
}

/// A grant of bytes from the parent's ring. Walk it via [`samples`](Self::samples).
pub struct NodeGrant<'a> {
    grant: ReadGrant<'a>,
    msg_size: usize,
}

impl NodeGrant<'_> {
    pub fn samples(&self) -> impl Iterator<Item = (Timestamp, &[u8])> + '_ {
        (0..self.sample_count()).map(|i| self.sample_at(i))
    }

    /// Number of complete `[Timestamp][value]` messages in this grant.
    pub fn sample_count(&self) -> usize {
        self.grant.len() / self.msg_size
    }

    /// Borrow the `i`-th sample in this grant. Panics if `i >= sample_count`.
    pub fn sample_at(&self, i: usize) -> (Timestamp, &[u8]) {
        let chunk = &self.grant[i * self.msg_size..(i + 1) * self.msg_size];
        let ts_bytes: [u8; 8] = chunk[..size_of::<Timestamp>()]
            .try_into()
            .expect("timestamp slice");
        let value = &chunk[size_of::<Timestamp>()..];
        (Timestamp::from_le_bytes(ts_bytes), value)
    }
}

/// Push a `[Timestamp][value bytes]` message into a ring. Drops on full —
/// matches the policy used by `Component::push_buf` (see
/// `libs/db/src/lib.rs:614`). Slow consumers lose samples, not the producer.
pub fn write_sample(disruptor: &Disruptor, ts: Timestamp, value_bytes: &[u8]) {
    let msg_size = size_of::<Timestamp>() + value_bytes.len();
    let Ok(mut grant) = disruptor.try_grant(msg_size) else {
        return;
    };
    grant[..size_of::<Timestamp>()].copy_from_slice(&ts.to_le_bytes());
    grant[size_of::<Timestamp>()..].copy_from_slice(value_bytes);
}

/// Concrete implementation behind every constructor. Owns the output ring
/// and a drop-guarded task handle so dropping the node cancels the task.
pub struct NodeImpl {
    id: NodeId,
    value_type: ValueType,
    output: Disruptor,
    _task: JoinHandleDropGuard<()>,
}

impl NodeImpl {
    /// Spawn a producer task and wrap it in a node.
    ///
    /// `build_task` receives the output ring and any captured parent `Arc`s
    /// (typically just by closing over them). The returned `Future` runs in
    /// `stellarator::spawn` and is cancelled when the node is dropped.
    pub fn spawn<F, Fut>(
        id: NodeId,
        value_type: ValueType,
        capacity: usize,
        build_task: F,
    ) -> Arc<Self>
    where
        F: FnOnce(Disruptor) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        Self::spawn_with_output(id, value_type, Disruptor::new(capacity), build_task)
    }

    /// Variant of [`spawn`](Self::spawn) that adopts an existing [`Disruptor`]
    /// as the output ring instead of allocating one. Used by `persist`, whose
    /// output is the underlying db Component's WAL.
    pub fn spawn_with_output<F, Fut>(
        id: NodeId,
        value_type: ValueType,
        output: Disruptor,
        build_task: F,
    ) -> Arc<Self>
    where
        F: FnOnce(Disruptor) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let task = stellarator::spawn(build_task(output.clone()));
        Arc::new(Self {
            id,
            value_type,
            output,
            _task: task.drop_guard(),
        })
    }
}

impl DynamicNode for NodeImpl {
    fn id(&self) -> NodeId {
        self.id
    }
    fn value_type(&self) -> &ValueType {
        &self.value_type
    }
    fn output(&self) -> &Disruptor {
        &self.output
    }
}

/// Tag bytes mixed into the hash to disambiguate constructors at the same
/// arity. Each picks a distinct constant; a collision would only produce a
/// different (still stable) `NodeId`, but distinct tags keep debug logs
/// readable.
pub mod op_tag {
    pub const FIXED_RATE_CLOCK: &[u8] = b"clock.fixed_rate";
    pub const PERSIST: &[u8] = b"persist";
    pub const FROM_DB: &[u8] = b"db_source";
    pub const EXPR_SYSTEM: &[u8] = b"expr.system";
    pub const EXPR_FIELD: &[u8] = b"expr.field";
}

/// Default ring capacity in bytes. Sized per-node from the value byte size
/// so generators with large schemas still get a reasonable buffer.
pub fn default_ring_bytes(value_bytes: usize) -> usize {
    let msg = size_of::<Timestamp>() + value_bytes;
    msg.saturating_mul(1024).max(8 * 1024)
}

/// Build a `NodeId` by hashing tag + parents + extra args. The `args`
/// callback folds every argument into the hasher in a stable order, using
/// fixed-width integers so the bytes don't depend on the platform.
pub fn hash_id(tag: &[u8], parents: &[NodeId], args: impl FnOnce(&mut StableHasher)) -> NodeId {
    let mut hasher = StableHasher::default();
    tag.hash(&mut hasher);
    for parent in parents {
        parent.0.hash(&mut hasher);
    }
    args(&mut hasher);
    NodeId(hasher.finish())
}

/// The hasher behind every [`NodeId`].
///
/// Node ids name on-disk components, so they must survive a toolchain
/// upgrade, which std's `DefaultHasher` does not promise. This runs the bytes
/// through the same FNV-1a as [`ComponentId::new`], high bit masked the same
/// way, so an id converts to a `ComponentId` as-is.
#[derive(Default)]
pub struct StableHasher(Vec<u8>);

impl Hasher for StableHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn finish(&self) -> u64 {
        const_fnv1a_hash::fnv1a_hash_64(&self.0, None) & !(1 << 63)
    }
}
