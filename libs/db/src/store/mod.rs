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
pub use peer::PeerStore;

use std::{future::Future, path::PathBuf, pin::Pin};

use metor_proto::types::{ComponentId, Timestamp};

use crate::{
    Error,
    append_log::AppendLog,
    seal::{SealRecord, SealRecordExt},
    time_series_2::{NODE_SIZE, TimeSeriesNode},
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
        // Each attempt stages into its own directory so concurrent fetches
        // of the same span can never delete or commit each other's files.
        static NEXT_STAGING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let attempt = NEXT_STAGING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = component_dir.join(format!("{}.{attempt}.fetching", seal.start_ts.0));
        std::fs::create_dir_all(&dir)?;
        let size = NODE_SIZE
            .max(seal.index_len + 64)
            .max(seal.data_len + 64);
        let index = AppendLog::with_size(size, dir.join("index"), seal.start_ts)?;
        let data = AppendLog::with_size(size, dir.join("data"), seal.element_size)?;
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

    /// Discard staged samples newer than `cover_end` and return the seal
    /// of what remains. This is how a coverage-trimmed span installs: the
    /// fetch moves the peer's whole node, but only the prefix this side
    /// is missing may land — the tail already exists as resident data.
    /// Call after the full payload verified against the peer's seal, then
    /// [`Self::commit`] with the returned seal.
    pub fn trim_to(&self, cover_end: Timestamp) -> Result<SealRecord, Error> {
        let node = TimeSeriesNode {
            index: self.index.clone(),
            data: self.data.clone(),
        };
        let keep = node.timestamps().partition_point(|ts| ts.0 <= cover_end.0);
        self.index
            .truncate((keep * size_of::<Timestamp>()) as u64);
        self.data
            .truncate((keep * node.element_size()) as u64);
        SealRecord::compute(&node)
            .ok_or_else(|| StoreError::Other("trim left an empty node".to_string()).into())
    }

    /// Verify the staged bytes against `seal` and promote the directory
    /// into place. Returns the final node directory and the (already
    /// mapped) node.
    pub fn commit(mut self, seal: &SealRecord) -> Result<(PathBuf, TimeSeriesNode), Error> {
        let node = TimeSeriesNode {
            index: self.index.clone(),
            data: self.data.clone(),
        };
        if !seal.verify(&node) {
            return Err(StoreError::ChecksumMismatch.into());
        }
        node.index.flush()?;
        node.data.flush()?;
        seal.write(&self.dir)?;
        let final_dir = self
            .dir
            .with_file_name(seal.start_ts.0.to_string());
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
    fn put<'a>(&'a self, key: NodeKey<'a>, node: SealedNode<'a>)
    -> BoxFuture<'a, Result<(), StoreError>>;

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

    fn contains<'a>(&'a self, key: NodeKey<'a>) -> BoxFuture<'a, Result<bool, StoreError>>;
}
