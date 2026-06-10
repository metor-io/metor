//! [`NodeStore`] over the `object_store` crate (S3, GCS, Azure, local
//! filesystem). Behind the `store-object` feature.
//!
//! `object_store` backends want a tokio runtime; metor-db runs on
//! stellarator. The adapter owns one dedicated tokio thread
//! ([`stellarator::struc_con::tokio`]) and ferries owned job payloads to
//! it over a channel — stellarator tasks never block on tokio and vice
//! versa. The copies this implies (a node's payload each way) are
//! background-only and bounded by one in-flight node.
//!
//! Layout: `<component_name>/<start_ts>/{index,data,seal}` under the
//! store the caller built. The `seal` object is written last and acts as
//! the commit marker — a node without one is invisible.

use std::sync::Arc;

use futures_lite::StreamExt as _;
use metor_proto::types::ComponentId;
use object_store::path::Path as ObjPath;
use stellarator::sync::WaitQueue;
use tokio::sync::mpsc;

use crate::seal::SealRecord;

use super::{BoxFuture, NodeFile, NodeKey, NodeStaging, NodeStore, SealedChunk, SealedNode, StoreError};

const CHUNK_BYTES: usize = 256 * 1024;

/// One-shot reply slot awaitable from stellarator and fillable from the
/// tokio thread.
struct Reply<T> {
    slot: std::sync::Mutex<Option<T>>,
    waker: WaitQueue,
}

impl<T> Reply<T> {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: std::sync::Mutex::new(None),
            waker: WaitQueue::new(),
        })
    }

    fn send(&self, value: T) {
        *self.slot.lock().unwrap() = Some(value);
        self.waker.wake_all();
    }

    async fn recv(&self) -> T {
        loop {
            if let Some(value) = self.slot.lock().unwrap().take() {
                return value;
            }
            let _ = self.waker.wait().await;
        }
    }
}

/// A node's seal plus its two payloads, fetched whole.
type FetchedNode = (SealRecord, Vec<u8>, Vec<u8>);

enum Job {
    Put {
        node_path: ObjPath,
        seal: Vec<u8>,
        index: Vec<u8>,
        data: Vec<u8>,
        reply: Arc<Reply<Result<(), StoreError>>>,
    },
    Get {
        node_path: ObjPath,
        reply: Arc<Reply<Result<FetchedNode, StoreError>>>,
    },
    List {
        component_path: ObjPath,
        reply: Arc<Reply<Result<Vec<SealRecord>, StoreError>>>,
    },
    Contains {
        node_path: ObjPath,
        checksum: u64,
        reply: Arc<Reply<Result<bool, StoreError>>>,
    },
}

pub struct ObjectStoreAdapter {
    jobs: mpsc::UnboundedSender<Job>,
}

impl ObjectStoreAdapter {
    /// Wrap any `object_store` backend the caller has built (e.g. an
    /// `AmazonS3Builder` product or `LocalFileSystem`).
    pub fn new(store: Arc<dyn object_store::ObjectStore>) -> Self {
        let (jobs, mut rx) = mpsc::unbounded_channel::<Job>();
        stellarator::struc_con::tokio(move |_cancel| async move {
            while let Some(job) = rx.recv().await {
                run_job(store.as_ref(), job).await;
            }
        });
        Self { jobs }
    }

    fn node_path(key: &NodeKey<'_>) -> ObjPath {
        ObjPath::from(format!("{}/{}", key.component_name, key.start_ts.0))
    }

    fn submit(&self, job: Job) -> Result<(), StoreError> {
        self.jobs
            .send(job)
            .map_err(|_| StoreError::Unavailable("object store thread exited".to_string()))
    }
}

async fn run_job(store: &dyn object_store::ObjectStore, job: Job) {
    match job {
        Job::Put {
            node_path,
            seal,
            index,
            data,
            reply,
        } => {
            let result = async {
                store
                    .put(&node_path.child("index"), index.into())
                    .await
                    .map_err(obj_err)?;
                store
                    .put(&node_path.child("data"), data.into())
                    .await
                    .map_err(obj_err)?;
                // Last: the commit marker.
                store
                    .put(&node_path.child("seal"), seal.into())
                    .await
                    .map_err(obj_err)?;
                Ok(())
            }
            .await;
            reply.send(result);
        }
        Job::Get { node_path, reply } => {
            let result = async {
                let seal = read_seal(store, &node_path).await?.ok_or(StoreError::NotFound)?;
                let index = read_all(store, &node_path.child("index")).await?;
                let data = read_all(store, &node_path.child("data")).await?;
                Ok((seal, index, data))
            }
            .await;
            reply.send(result);
        }
        Job::List {
            component_path,
            reply,
        } => {
            let result = async {
                let mut seals = Vec::new();
                let mut listing = store.list(Some(&component_path));
                while let Some(meta) = listing.next().await {
                    let meta = meta.map_err(obj_err)?;
                    if meta.location.filename() != Some("seal") {
                        continue;
                    }
                    let bytes = store
                        .get(&meta.location)
                        .await
                        .map_err(obj_err)?
                        .bytes()
                        .await
                        .map_err(obj_err)?;
                    seals.push(
                        postcard::from_bytes::<SealRecord>(&bytes)
                            .map_err(|e| StoreError::Other(e.to_string()))?,
                    );
                }
                seals.sort_unstable_by_key(|s| s.start_ts.0);
                Ok(seals)
            }
            .await;
            reply.send(result);
        }
        Job::Contains {
            node_path,
            checksum,
            reply,
        } => {
            let result = read_seal(store, &node_path)
                .await
                .map(|seal| seal.is_some_and(|s| s.checksum == checksum));
            reply.send(result);
        }
    }
}

async fn read_seal(
    store: &dyn object_store::ObjectStore,
    node_path: &ObjPath,
) -> Result<Option<SealRecord>, StoreError> {
    match store.get(&node_path.child("seal")).await {
        Ok(result) => {
            let bytes = result.bytes().await.map_err(obj_err)?;
            Ok(Some(
                postcard::from_bytes(&bytes).map_err(|e| StoreError::Other(e.to_string()))?,
            ))
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(err) => Err(obj_err(err)),
    }
}

async fn read_all(
    store: &dyn object_store::ObjectStore,
    path: &ObjPath,
) -> Result<Vec<u8>, StoreError> {
    match store.get(path).await {
        Ok(result) => Ok(result.bytes().await.map_err(obj_err)?.to_vec()),
        Err(object_store::Error::NotFound { .. }) => Err(StoreError::NotFound),
        Err(err) => Err(obj_err(err)),
    }
}

fn obj_err(err: object_store::Error) -> StoreError {
    StoreError::Unavailable(err.to_string())
}

impl NodeStore for ObjectStoreAdapter {
    fn put<'a>(
        &'a self,
        key: NodeKey<'a>,
        node: SealedNode<'a>,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let seal = postcard::to_allocvec(&node.seal)
                .map_err(|e| StoreError::Other(e.to_string()))?;
            let reply = Reply::new();
            self.submit(Job::Put {
                node_path: Self::node_path(&key),
                seal,
                index: node.index.to_vec(),
                data: node.data.to_vec(),
                reply: reply.clone(),
            })?;
            reply.recv().await
        })
    }

    fn get<'a>(
        &'a self,
        key: NodeKey<'a>,
        dst: &'a NodeStaging,
    ) -> BoxFuture<'a, Result<SealRecord, StoreError>> {
        Box::pin(async move {
            let reply = Reply::new();
            self.submit(Job::Get {
                node_path: Self::node_path(&key),
                reply: reply.clone(),
            })?;
            let (seal, index, data) = reply.recv().await?;
            for (file, bytes) in [(NodeFile::Index, &index), (NodeFile::Data, &data)] {
                for (i, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
                    dst.append(&SealedChunk {
                        file,
                        offset: (i * CHUNK_BYTES) as u64,
                        bytes: chunk,
                    })
                    .map_err(|e| StoreError::Other(e.to_string()))?;
                }
            }
            Ok(seal)
        })
    }

    fn list<'a>(
        &'a self,
        _component_id: ComponentId,
        component_name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SealRecord>, StoreError>> {
        Box::pin(async move {
            let reply = Reply::new();
            self.submit(Job::List {
                component_path: ObjPath::from(component_name),
                reply: reply.clone(),
            })?;
            reply.recv().await
        })
    }

    fn contains<'a>(&'a self, key: NodeKey<'a>) -> BoxFuture<'a, Result<bool, StoreError>> {
        Box::pin(async move {
            let reply = Reply::new();
            self.submit(Job::Contains {
                node_path: Self::node_path(&key),
                checksum: key.checksum,
                reply: reply.clone(),
            })?;
            reply.recv().await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[stellarator::test]
    async fn object_store_adapter_conforms() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("objects");
        std::fs::create_dir_all(&backing).unwrap();
        let store = ObjectStoreAdapter::new(Arc::new(
            object_store::local::LocalFileSystem::new_with_prefix(&backing).unwrap(),
        ));
        crate::store::conformance::run(&store, dir.path()).await;
    }
}
