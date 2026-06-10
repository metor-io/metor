use std::{
    fs::{self, File},
    io::Write as _,
    path::PathBuf,
};

use metor_proto::types::ComponentId;

use crate::{
    Error,
    seal::{SEAL_FILE, SealRecord, SealRecordExt as _},
};

use super::{BoxFuture, NodeFile, NodeKey, NodeStaging, NodeStore, SealedNode, StoreError};

/// Reference [`NodeStore`] backed by a local directory tree:
/// `root/<component_id>/<start_ts>/{seal,index,data}`, payloads stored
/// raw. Doubles as the cheapest real archive (e.g. a mounted NAS path)
/// and as the fixture backend for the conformance suite.
pub struct LocalDirStore {
    root: PathBuf,
}

impl LocalDirStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn node_dir(&self, component_id: ComponentId, start_ts: i64) -> PathBuf {
        self.root
            .join(component_id.0.to_string())
            .join(start_ts.to_string())
    }

    fn read_seal(&self, key: &NodeKey<'_>) -> Result<Option<SealRecord>, StoreError> {
        let dir = self.node_dir(key.component_id, key.start_ts.0);
        SealRecord::read(&dir).map_err(|err| match err {
            Error::Io(err) => StoreError::Io(err),
            err => StoreError::Other(err.to_string()),
        })
    }
}

impl NodeStore for LocalDirStore {
    fn put<'a>(
        &'a self,
        key: NodeKey<'a>,
        node: SealedNode<'a>,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            if let Some(existing) = self.read_seal(&key)?
                && existing.checksum == node.seal.checksum
            {
                return Ok(());
            }
            let final_dir = self.node_dir(key.component_id, key.start_ts.0);
            let parent = final_dir.parent().expect("node dir always has a parent");
            fs::create_dir_all(parent)?;
            let tmp = final_dir.with_extension("tmp");
            if tmp.exists() {
                fs::remove_dir_all(&tmp)?;
            }
            fs::create_dir_all(&tmp)?;
            let write_file = |name: &str, bytes: &[u8]| -> Result<(), StoreError> {
                let mut file = File::create(tmp.join(name))?;
                file.write_all(bytes)?;
                file.sync_all()?;
                Ok(())
            };
            write_file("index", node.index)?;
            write_file("data", node.data)?;
            write_file(
                SEAL_FILE,
                &postcard::to_allocvec(&node.seal).map_err(|e| StoreError::Other(e.to_string()))?,
            )?;
            if final_dir.exists() {
                fs::remove_dir_all(&final_dir)?;
            }
            fs::rename(&tmp, &final_dir)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })
    }

    fn get<'a>(
        &'a self,
        key: NodeKey<'a>,
        dst: &'a NodeStaging,
    ) -> BoxFuture<'a, Result<SealRecord, StoreError>> {
        Box::pin(async move {
            let seal = self.read_seal(&key)?.ok_or(StoreError::NotFound)?;
            let dir = self.node_dir(key.component_id, key.start_ts.0);
            for (file, name) in [(NodeFile::Index, "index"), (NodeFile::Data, "data")] {
                let bytes = fs::read(dir.join(name))?;
                dst.append_file(file, &bytes)?;
            }
            Ok(seal)
        })
    }

    fn list<'a>(
        &'a self,
        component_id: ComponentId,
        _component_name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SealRecord>, StoreError>> {
        Box::pin(async move {
            let dir = self.root.join(component_id.0.to_string());
            let mut seals = Vec::new();
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(seals),
                Err(err) => return Err(err.into()),
            };
            for entry in entries {
                let path = entry?.path();
                let seal_path = path.join(SEAL_FILE);
                if !path.is_dir() || !seal_path.exists() {
                    continue;
                }
                let buf = fs::read(seal_path)?;
                seals.push(
                    postcard::from_bytes(&buf).map_err(|e| StoreError::Other(e.to_string()))?,
                );
            }
            seals.sort_unstable_by_key(|s: &SealRecord| s.start_ts.0);
            Ok(seals)
        })
    }

    fn contains<'a>(&'a self, key: NodeKey<'a>) -> BoxFuture<'a, Result<bool, StoreError>> {
        Box::pin(async move {
            Ok(self
                .read_seal(&key)?
                .is_some_and(|seal| seal.checksum == key.checksum))
        })
    }
}
