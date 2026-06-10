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
//! Layout: `<component_id>/<start_ts>/{index,data,seal}` under the
//! store the caller built. The `seal` object is written last and acts as
//! the commit marker — a node without one is invisible.

use std::sync::Arc;

use futures_lite::StreamExt as _;
use metor_proto::types::ComponentId;
use object_store::path::Path as ObjPath;
use stellarator::util::{OneshotTx, oneshot};
use tokio::sync::mpsc;

use crate::seal::{SEAL_FILE, SealRecord};

use super::{BoxFuture, CHUNK_BYTES, NodeFile, NodeKey, NodeStaging, NodeStore, SealedNode, StoreError};

/// A node's seal plus its two payloads, fetched whole.
type FetchedNode = (SealRecord, Vec<u8>, Vec<u8>);

enum Job {
    Put {
        node_path: ObjPath,
        seal: Vec<u8>,
        index: Vec<u8>,
        data: Vec<u8>,
        reply: OneshotTx<Result<(), StoreError>>,
    },
    Get {
        node_path: ObjPath,
        reply: OneshotTx<Result<FetchedNode, StoreError>>,
    },
    List {
        component_path: ObjPath,
        reply: OneshotTx<Result<Vec<SealRecord>, StoreError>>,
    },
    Contains {
        node_path: ObjPath,
        checksum: u64,
        reply: OneshotTx<Result<bool, StoreError>>,
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
        ObjPath::from(format!("{}/{}", key.component_id.0, key.start_ts.0))
    }

    fn submit(&self, job: Job) -> Result<(), StoreError> {
        self.jobs.send(job).map_err(|_| thread_exited())
    }
}

fn thread_exited() -> StoreError {
    StoreError::Unavailable("object store thread exited".to_string())
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
                    .put(&node_path.child(SEAL_FILE), seal.into())
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
                    if meta.location.filename() != Some(SEAL_FILE) {
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
    match store.get(&node_path.child(SEAL_FILE)).await {
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
            let (reply, rx) = oneshot();
            self.submit(Job::Put {
                node_path: Self::node_path(&key),
                seal,
                index: node.index.to_vec(),
                data: node.data.to_vec(),
                reply,
            })?;
            rx.wait().await.ok_or_else(thread_exited)?
        })
    }

    fn get<'a>(
        &'a self,
        key: NodeKey<'a>,
        dst: &'a NodeStaging,
    ) -> BoxFuture<'a, Result<SealRecord, StoreError>> {
        Box::pin(async move {
            let (reply, rx) = oneshot();
            self.submit(Job::Get {
                node_path: Self::node_path(&key),
                reply,
            })?;
            let (seal, index, data) = rx.wait().await.ok_or_else(thread_exited)??;
            dst.append_file(NodeFile::Index, &index)?;
            dst.append_file(NodeFile::Data, &data)?;
            Ok(seal)
        })
    }

    fn list<'a>(
        &'a self,
        component_id: ComponentId,
        _component_name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SealRecord>, StoreError>> {
        Box::pin(async move {
            let (reply, rx) = oneshot();
            self.submit(Job::List {
                component_path: ObjPath::from(component_id.0.to_string()),
                reply,
            })?;
            rx.wait().await.ok_or_else(thread_exited)?
        })
    }

    fn contains<'a>(&'a self, key: NodeKey<'a>) -> BoxFuture<'a, Result<bool, StoreError>> {
        Box::pin(async move {
            let (reply, rx) = oneshot();
            self.submit(Job::Contains {
                node_path: Self::node_path(&key),
                checksum: key.checksum,
                reply,
            })?;
            rx.wait().await.ok_or_else(thread_exited)?
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
