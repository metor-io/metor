//! Pluggable backends for archiving and fetching sealed nodes.
//!
//! The unit of exchange is one sealed node: `(component, start_ts)` →
//! byte-identical committed `index`/`data` payloads plus the
//! [`SealRecord`] that names and checksums them. Everything above this
//! trait — tiering, hydration, the manifest — is backend-agnostic; a peer
//! metor-db, an object store, or a user-written HTTP/ClickHouse backend
//! all look the same.
//!
//! Implementations are validated with [`conformance`], which any
//! third-party backend should run in its own tests.

pub mod conformance;
mod local_dir;
#[cfg(feature = "store-object")]
mod object;
mod peer;

pub use local_dir::LocalDirStore;
#[cfg(feature = "store-object")]
pub use object::ObjectStoreAdapter;
pub use peer::{MAX_BATCH, PeerStore};

use std::{future::Future, path::PathBuf, pin::Pin};

use metor_proto::types::{ComponentId, Timestamp};
use metor_proto_wkt::GetTimeSeries;
use zerocopy::IntoBytes;

use crate::{
    Error,
    append_log::AppendLog,
    seal::{SealRecord, SealRecordExt},
    time_series::{NODE_SIZE, TimeSeriesNode},
};

/// Store futures run on background tasks of a local executor; they do not
/// need to be `Send`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io {0}")]
    Io(#[from] std::io::Error),
    #[error("node not found in store")]
    NotFound,
    #[error("checksum mismatch")]
    ChecksumMismatch,
    #[error("store unavailable: {0}")]
    Unavailable(String),
    /// The backend cannot answer this kind of request at all. Callers
    /// should stop asking rather than retry.
    #[error("unsupported by this store")]
    Unsupported,
    #[error("{0}")]
    Other(String),
}

/// Identity of one sealed node in a store. `component_id` is the stable
/// key; `component_name` is provided for human-addressable layouts (S3
/// prefixes, HTTP paths). `checksum` pins the exact bytes — a store may
/// use it to short-circuit idempotent re-puts. `schema` rides along so
/// archive-style stores can create the component on first contact.
#[derive(Debug, Clone, Copy)]
pub struct NodeKey<'a> {
    pub component_id: ComponentId,
    pub component_name: &'a str,
    pub schema: &'a metor_proto::schema::Schema<Vec<u64>>,
    pub start_ts: Timestamp,
    pub checksum: u64,
}

/// Which of a sealed node's two payload files a chunk belongs to. The
/// discriminant is shared with the wire protocol so peer transfer needs
/// no conversion.
pub use metor_proto_wkt::NodeFileKind as NodeFile;

pub(crate) const CHUNK_BYTES: usize = 256 * 1024;

/// One streamable piece of a sealed node's payload.
#[derive(Debug, Clone, Copy)]
pub struct SealedChunk<'a> {
    pub file: NodeFile,
    pub offset: u64,
    pub bytes: &'a [u8],
}

/// Zero-copy view of a resident sealed node — the committed payloads only,
/// never the sparse tail. This is what `put` ships; impls can take the
/// whole slices or stream via [`Self::chunks`] without buffering 32MB.
#[derive(Debug, Clone, Copy)]
pub struct SealedNode<'a> {
    pub seal: SealRecord,
    pub index: &'a [u8],
    pub data: &'a [u8],
}

impl<'a> SealedNode<'a> {
    /// View `node` through its seal. Returns `None` when the node's
    /// committed bytes no longer match the seal (it was never valid to
    /// ship, e.g. an unsealed head).
    pub fn from_node(node: &'a TimeSeriesNode, seal: SealRecord) -> Option<Self> {
        if !seal.verify(node) {
            return None;
        }
        Some(Self {
            seal,
            index: node.index.data(),
            data: node.data.data(),
        })
    }

    pub fn chunks(&self, chunk_bytes: usize) -> impl Iterator<Item = SealedChunk<'a>> + 'a {
        let chunked = |file: NodeFile, bytes: &'a [u8]| {
            bytes
                .chunks(chunk_bytes)
                .enumerate()
                .map(move |(i, bytes)| SealedChunk {
                    file,
                    offset: (i * chunk_bytes) as u64,
                    bytes,
                })
        };
        chunked(NodeFile::Index, self.index).chain(chunked(NodeFile::Data, self.data))
    }
}

/// In-progress download of one node, staged as real [`AppendLog`] files in
/// a `<start_ts>.<n>.fetching` directory beside the component's live
/// nodes. Stores append payload chunks in order; [`Self::commit`] verifies
/// the checksum and atomically promotes the directory to `<start_ts>/`, so
/// a crash at any point leaves either nothing visible or a complete,
/// verified node.
pub struct NodeStaging {
    dir: PathBuf,
    index: AppendLog<Timestamp>,
    data: AppendLog<u64>,
    committed: bool,
}

impl NodeStaging {
    pub fn create(component_dir: &std::path::Path, seal: &SealRecord) -> Result<Self, Error> {
        Self::sized(
            component_dir,
            seal.start_ts,
            seal.element_size,
            seal.index_len,
            seal.data_len,
        )
    }

    /// Stage a node that is being built sample by sample rather than
    /// downloaded: there is no seal yet, so the files are sized from an
    /// expected sample count. `samples` is only a capacity hint — writes
    /// past it fail with [`Error::MapOverflow`], and a batch that does not
    /// fit one node belongs in several.
    pub fn with_capacity(
        component_dir: &std::path::Path,
        start_ts: Timestamp,
        element_size: u64,
        samples: usize,
    ) -> Result<Self, Error> {
        let samples = samples as u64;
        Self::sized(
            component_dir,
            start_ts,
            element_size,
            samples * size_of::<Timestamp>() as u64,
            samples * element_size,
        )
    }

    fn sized(
        component_dir: &std::path::Path,
        start_ts: Timestamp,
        element_size: u64,
        index_len: u64,
        data_len: u64,
    ) -> Result<Self, Error> {
        // Each attempt stages into its own directory so concurrent fetches
        // of the same span can never delete or commit each other's files.
        static NEXT_STAGING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let attempt = NEXT_STAGING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = component_dir.join(format!("{}.{attempt}.fetching", start_ts.0));
        std::fs::create_dir_all(&dir)?;
        let size = NODE_SIZE.max(index_len + 64).max(data_len + 64);
        let index = AppendLog::with_size(size, dir.join("index"), start_ts)?;
        let data = AppendLog::with_size(size, dir.join("data"), element_size)?;
        Ok(Self {
            dir,
            index,
            data,
            committed: false,
        })
    }

    pub fn append(&self, chunk: &SealedChunk<'_>) -> Result<(), Error> {
        let log_len = match chunk.file {
            NodeFile::Index => {
                self.index.write(chunk.bytes)?;
                self.index.len()
            }
            NodeFile::Data => {
                self.data.write(chunk.bytes)?;
                self.data.len()
            }
        };
        // Chunks must arrive in order; the append log has no holes.
        if log_len != chunk.offset + chunk.bytes.len() as u64 {
            return Err(StoreError::Other("out of order chunk".to_string()).into());
        }
        Ok(())
    }

    /// Append a whole payload as ordered [`CHUNK_BYTES`]-sized chunks.
    pub fn append_file(&self, file: NodeFile, bytes: &[u8]) -> Result<(), StoreError> {
        for (i, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            self.append(&SealedChunk {
                file,
                offset: (i * CHUNK_BYTES) as u64,
                bytes: chunk,
            })
            .map_err(|e| StoreError::Other(e.to_string()))?;
        }
        Ok(())
    }

    /// Append one sample. The counterpart to [`Self::append`] for nodes
    /// composed locally instead of downloaded: no seal exists yet to
    /// verify against, so ordering is the caller's contract.
    pub fn push(&self, ts: Timestamp, bytes: &[u8]) -> Result<(), Error> {
        self.index.write(&ts.to_le_bytes())?;
        self.data.write(bytes)?;
        Ok(())
    }

    /// Summarize the staged bytes as they stand. `None` when nothing has
    /// been staged — an empty node is never worth committing.
    pub fn seal(&self) -> Option<SealRecord> {
        SealRecord::compute(&self.node())
    }

    /// Copy the staged samples stamped within `cover` into a node of their
    /// own, returning it with its seal, or `None` when none fall inside.
    /// This is how a remote seal partly overlapped by local data installs:
    /// the fetch moves the peer's whole node, but only the pieces this side
    /// is missing may land. Call after the full payload verified against
    /// the peer's seal, then [`Self::commit`] the piece with its seal.
    pub fn cut(
        &self,
        cover: std::ops::RangeInclusive<Timestamp>,
    ) -> Result<Option<(NodeStaging, SealRecord)>, Error> {
        let node = self.node();
        let timestamps = node.timestamps();
        let lo = timestamps.partition_point(|ts| ts.0 < cover.start().0);
        let hi = timestamps.partition_point(|ts| ts.0 <= cover.end().0);
        if lo >= hi {
            return Ok(None);
        }
        let size = node.element_size();
        let component_dir = self
            .dir
            .parent()
            .ok_or_else(|| StoreError::Other("staging has no component dir".to_string()))?;
        Self::of_samples(
            component_dir,
            &timestamps[lo..hi],
            size,
            &node.data.data()[lo * size..hi * size],
        )
        .map(Some)
    }

    /// Stage `timestamps`, in order and at least one, with their samples
    /// `data` as one node, returning it with its seal.
    pub(crate) fn of_samples(
        component_dir: &std::path::Path,
        timestamps: &[Timestamp],
        element_size: usize,
        data: &[u8],
    ) -> Result<(NodeStaging, SealRecord), Error> {
        let staging = Self::with_capacity(
            component_dir,
            timestamps[0],
            element_size as u64,
            timestamps.len(),
        )?;
        staging.index.write(timestamps.as_bytes())?;
        staging.data.write(data)?;
        let seal = staging.seal().expect("staged at least one sample");
        Ok((staging, seal))
    }

    /// The staging directory, which [`Self::commit`] renames into place
    /// with everything in it.
    pub(crate) fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn node(&self) -> TimeSeriesNode {
        TimeSeriesNode::from_logs(self.index.clone(), self.data.clone())
    }

    /// Verify the staged bytes against `seal` and promote the directory
    /// into place. Returns the final node directory and the (already
    /// mapped) node.
    pub fn commit(mut self, seal: &SealRecord) -> Result<(PathBuf, TimeSeriesNode), Error> {
        let final_dir = self.dir.with_file_name(seal.start_ts.0.to_string());
        let node = self.node();

        if !seal.verify(&node) {
            return Err(StoreError::ChecksumMismatch.into());
        }
        node.index.flush()?;
        node.data.flush()?;
        seal.write(&self.dir)?;
        if final_dir.exists() {
            return Err(StoreError::Other(format!(
                "node {} was committed by a concurrent fetch",
                seal.start_ts.0
            ))
            .into());
        }
        std::fs::rename(&self.dir, &final_dir)?;
        if let Some(parent) = final_dir.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        self.committed = true;
        Ok((final_dir, node))
    }

    /// Move a range-fetch fragment into place, in a directory named apart
    /// from sealed nodes (`<start_ts>.<n>.fragment`): its first sample may
    /// be where a whole node will later land. Opening the component deletes
    /// fragments rather than loading them, so unlike [`Self::commit`] this
    /// neither verifies nor syncs anything.
    pub(crate) fn commit_fragment(mut self) -> Result<(PathBuf, TimeSeriesNode), Error> {
        let final_dir = self.dir.with_extension("fragment");
        std::fs::rename(&self.dir, &final_dir)?;
        self.committed = true;
        let mut node = self.node();
        node.fragment = true;
        Ok((final_dir, node))
    }
}

impl Drop for NodeStaging {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// A backend that can hold sealed nodes durably. Implementations must be
/// usable as `Arc<dyn NodeStore>`; methods return boxed futures so the
/// trait stays object-safe on stable.
///
/// Contract:
/// - `put` returning `Ok` means the bytes are durable in the backend —
///   never ack before persisting. Re-putting an existing node must be
///   idempotent (the checksum identifies the bytes).
/// - `get` must reproduce the payloads byte-identically; the caller
///   verifies against the seal before installing, so a corrupt store
///   fails loudly rather than silently.
/// - How a backend lays data out internally is its own concern (an object
///   store may keep files, a database may explode samples into rows) as
///   long as the round trip is exact.
pub trait NodeStore: Send + Sync + 'static {
    fn put<'a>(
        &'a self,
        key: NodeKey<'a>,
        node: SealedNode<'a>,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    /// Stream the node's payloads into `dst` (in order, per file) and
    /// return the store's seal record for it.
    fn get<'a>(
        &'a self,
        key: NodeKey<'a>,
        dst: &'a NodeStaging,
    ) -> BoxFuture<'a, Result<SealRecord, StoreError>>;

    /// Every sealed node the store holds for this component, sorted by
    /// start timestamp.
    fn list<'a>(
        &'a self,
        component_id: ComponentId,
        component_name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SealRecord>, StoreError>>;

    /// [`Self::list`], plus the unsealed head a live database holds past
    /// its sealed nodes, taken from the same snapshot. Stores of sealed
    /// nodes have no head.
    fn listing<'a>(
        &'a self,
        component_id: ComponentId,
        component_name: &'a str,
    ) -> BoxFuture<'a, Result<Listing, StoreError>> {
        Box::pin(async move {
            Ok(Listing {
                nodes: self.list(component_id, component_name).await?,
                head: None,
            })
        })
    }

    fn contains<'a>(&'a self, key: NodeKey<'a>) -> BoxFuture<'a, Result<bool, StoreError>>;

    /// Answer range fetches ([`GetTimeSeries`]) instead of whole nodes, one
    /// reply (or error) per request, in order. Only a live database can:
    /// the default, for stores of sealed nodes, is [`StoreError::Unsupported`],
    /// as it is from a peer too old to say what a reply covers. Callers then
    /// fall back to whole nodes.
    fn time_series<'a>(
        &'a self,
        requests: &'a [GetTimeSeries],
    ) -> BoxFuture<'a, Result<Vec<Result<SeriesReply, StoreError>>, StoreError>> {
        let _ = requests;
        Box::pin(async { Err(StoreError::Unsupported) })
    }
}

/// Everything a store can give back for one component: its sealed nodes,
/// sorted by start, and the unsealed head a live database holds past them.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub nodes: Vec<SealRecord>,
    pub head: Option<metor_proto_wkt::UnsealedHead>,
}

/// One reply to a range fetch, owning its buffer.
pub type SeriesReply = metor_proto::types::OwnedTimeSeries<stellarator::buf::Slice<Vec<u8>>>;
