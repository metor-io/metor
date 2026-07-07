use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::sync::Arc;

use metor_db::ComponentSchema;
use metor_db::disruptor::{Disruptor, ReadGrant, Reader};
use metor_proto::types::{ComponentId, PrimType, Timestamp};
use smallvec::SmallVec;
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

/// Validate that `node` is clock-typed.
pub fn require_clock(node: &std::sync::Arc<dyn DynamicNode>) -> Result<(), BuildError> {
    match node.value_type() {
        ValueType::Clock => Ok(()),
        v => Err(BuildError::ExpectedClock(v.clone())),
    }
}

/// Errors surfaced when *building* a node — schema mismatches, clock
/// mismatches, or wrong value type for an op. The node editor will render
/// these as edge errors in a later phase.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("expected a clock input, got {0:?}")]
    ExpectedClock(ValueType),
    #[error("expected a value input, got Clock")]
    ExpectedValue,
    #[error("schema mismatch: {a:?} vs {b:?}")]
    SchemaMismatch {
        a: ComponentSchema,
        b: ComponentSchema,
    },
    #[error("composer inputs must share a clock (parent_clock_id mismatch)")]
    ClockMismatch,
    #[error("shapes are not broadcast-compatible: {a:?} vs {b:?}")]
    BroadcastMismatch {
        a: SmallVec<[usize; 4]>,
        b: SmallVec<[usize; 4]>,
    },
    #[error("{op} does not support dtype {dtype:?}")]
    UnsupportedDtype { op: &'static str, dtype: PrimType },
    #[error("composer needs at least one input")]
    EmptyInputs,
    #[error("graph contains a cycle")]
    Cycle,
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
}

/// A live producer task whose output is a fan-out [`Disruptor`].
///
/// Drop the last `Arc<dyn DynamicNode>` to cancel the task: the node holds a
/// `JoinHandleDropGuard` which calls `cancel` on drop.
pub trait DynamicNode: Send + Sync + 'static {
    fn id(&self) -> NodeId;
    fn value_type(&self) -> &ValueType;
    /// The clock id this node ticks against, if any. Composers require their
    /// inputs to share a clock id; resamplers reset it to the target clock.
    fn parent_clock_id(&self) -> Option<NodeId>;
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

/// Internal reader handed to derivation/composer/resampler tasks. Yields
/// every sample in order — the opposite of [`crate::WalComponentStream`],
/// which collapses to the latest. View bridging goes through
/// [`crate::WalComponentStream::from_disruptor`] instead.
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

    /// Construct a reader directly over an arbitrary `[Timestamp][value]`
    /// framed `Disruptor`. Used by `from_db` to read off a Component's
    /// WAL without spawning a wrapper node.
    pub fn from_disruptor(disruptor: &Disruptor, value_bytes: usize) -> Self {
        Self::new(disruptor, value_bytes)
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
        let msg_size = self.msg_size;
        self.grant.chunks_exact(msg_size).map(move |chunk| {
            let ts_bytes: [u8; 8] = chunk[..size_of::<Timestamp>()]
                .try_into()
                .expect("timestamp slice");
            let value = &chunk[size_of::<Timestamp>()..];
            (Timestamp::from_le_bytes(ts_bytes), value)
        })
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
    parent_clock_id: Option<NodeId>,
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
        parent_clock_id: Option<NodeId>,
        capacity: usize,
        build_task: F,
    ) -> Arc<Self>
    where
        F: FnOnce(Disruptor) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        Self::spawn_with_output(
            id,
            value_type,
            parent_clock_id,
            Disruptor::new(capacity),
            build_task,
        )
    }

    /// Variant of [`spawn`](Self::spawn) that adopts an existing [`Disruptor`]
    /// as the output ring instead of allocating one. Used by `persist`, whose
    /// output is the underlying db Component's WAL.
    pub fn spawn_with_output<F, Fut>(
        id: NodeId,
        value_type: ValueType,
        parent_clock_id: Option<NodeId>,
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
            parent_clock_id,
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
    fn parent_clock_id(&self) -> Option<NodeId> {
        self.parent_clock_id
    }
    fn output(&self) -> &Disruptor {
        &self.output
    }
}

/// Tag bytes mixed into the hash to disambiguate ops at the same arity. Each
/// op picks a distinct constant; collisions just produce a different (still
/// stable) NodeId, but using distinct tags keeps debug logs readable.
pub mod op_tag {
    pub const FIXED_RATE_CLOCK: &[u8] = b"clock.fixed_rate";
    pub const CLOCK_OF: &[u8] = b"clock.of";
    pub const WAVEFORM: &[u8] = b"gen.waveform";
    pub const RANDOM: &[u8] = b"gen.random";
    pub const CONSTANT: &[u8] = b"gen.constant";
    pub const SCALE: &[u8] = b"derive.scale";
    pub const OFFSET: &[u8] = b"derive.offset";
    pub const ABS: &[u8] = b"derive.abs";
    pub const NEG: &[u8] = b"derive.neg";
    pub const LOG: &[u8] = b"derive.log";
    pub const SQRT: &[u8] = b"derive.sqrt";
    pub const EXP: &[u8] = b"derive.exp";
    pub const FLOOR: &[u8] = b"derive.floor";
    pub const WINDOW: &[u8] = b"derive.window";
    pub const FFT: &[u8] = b"derive.fft";
    pub const ADD: &[u8] = b"compose.add";
    pub const SUB: &[u8] = b"compose.sub";
    pub const MUL: &[u8] = b"compose.mul";
    pub const DIV: &[u8] = b"compose.div";
    pub const MEAN: &[u8] = b"compose.mean";
    pub const PACK: &[u8] = b"compose.pack";
    pub const DOT: &[u8] = b"compose.dot";
    pub const MAGNITUDE: &[u8] = b"derive.magnitude";
    pub const INDEX: &[u8] = b"derive.index";
    pub const THRESHOLD: &[u8] = b"derive.threshold";
    pub const DELTA: &[u8] = b"derive.delta";
    pub const DELTA_T: &[u8] = b"derive.delta_t";
    pub const ZOH: &[u8] = b"resample.zoh";
    pub const LINEAR: &[u8] = b"resample.linear";
    pub const LATEST_AT: &[u8] = b"resample.latest_at";
    pub const PERSIST: &[u8] = b"persist";
    pub const FROM_DB: &[u8] = b"db_source";
}

/// Default ring capacity in bytes. Sized per-node from the value byte size
/// so generators with large schemas still get a reasonable buffer.
pub fn default_ring_bytes(value_bytes: usize) -> usize {
    let msg = size_of::<Timestamp>() + value_bytes;
    msg.saturating_mul(1024).max(8 * 1024)
}

/// Convenience: build a `NodeId` by hashing tag + parents + extra args. The
/// `args` callback should fold every argument into the hasher in a stable
/// order. We use `std::hash::DefaultHasher` (SipHash) — same family the rest
/// of the panel uses; the absolute value is opaque, only equality matters.
pub fn hash_id(
    tag: &[u8],
    parents: &[NodeId],
    args: impl FnOnce(&mut std::collections::hash_map::DefaultHasher),
) -> NodeId {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tag.hash(&mut hasher);
    for parent in parents {
        parent.0.hash(&mut hasher);
    }
    args(&mut hasher);
    NodeId(hasher.finish())
}
