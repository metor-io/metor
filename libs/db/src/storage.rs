//! How much disk a mirroring client may use, shared by every host of a
//! mirror (the panel, the fsw3 `Db` link, the db CLI) so they read one
//! configuration the same way.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::evict::Eviction;
use crate::peer_cache::PeerCache;

/// A mirror's disk policy: what it evicts while running, and how much the
/// per-peer caches it leaves behind may hold across sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// What the open database does with history nobody is reading.
    pub eviction: Eviction,
    /// Where per-peer caches live; [`PeerCache::default_root`] when unset.
    pub cache_dir: Option<PathBuf>,
    /// The most every cached peer database together may occupy on disk;
    /// [`PeerCache::prune`] removes the least recently used past it.
    pub cache_max_bytes: u64,
    /// How long a peer can go unseen before its cache is removed.
    pub cache_max_idle: Duration,
}

impl StorageConfig {
    pub const DEFAULT_CACHE_MAX_BYTES: u64 = 20 << 30;
    pub const DEFAULT_CACHE_MAX_IDLE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

    /// The peer cache this config points at, or `None` when there is no
    /// directory for it: no `cache_dir` and no user cache directory.
    pub fn peer_cache(&self) -> Option<PeerCache> {
        self.cache_dir
            .clone()
            .or_else(PeerCache::default_root)
            .map(PeerCache::new)
    }

    /// [`PeerCache::prune`] under this config's limits.
    pub fn prune_cache(
        &self,
        cache: &PeerCache,
        skip: Option<metor_proto_wkt::InstanceId>,
    ) -> Vec<PathBuf> {
        cache.prune(self.cache_max_bytes, self.cache_max_idle, skip)
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            eviction: Eviction::default(),
            cache_dir: None,
            cache_max_bytes: Self::DEFAULT_CACHE_MAX_BYTES,
            cache_max_idle: Self::DEFAULT_CACHE_MAX_IDLE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let config = StorageConfig {
            eviction: Eviction::Budget { max_bytes: 1 << 30 },
            cache_dir: Some("/tmp/peers".into()),
            ..Default::default()
        };
        let bytes = postcard::to_allocvec(&config).unwrap();
        assert_eq!(
            postcard::from_bytes::<StorageConfig>(&bytes).unwrap(),
            config
        );
        assert_eq!(
            StorageConfig::default().cache_max_bytes,
            20 * 1024 * 1024 * 1024
        );
    }
}
