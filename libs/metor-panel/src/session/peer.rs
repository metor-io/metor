//! The database a session mirroring one peer runs on, chosen after asking
//! the peer who it is.
//!
//! A mirror of a long-running metor-db keeps its DB in the per-peer cache
//! ([`PeerCache`]) and reopens it on the next connection to the same peer,
//! so history, LoD levels, metadata, seeded manifests and expression outputs
//! are already there. That needs the peer's [`InstanceId`] before the DB
//! exists, so the session probes the address first. Anything that can't
//! name a cache — an fsw link, an ephemeral gateway, a peer that doesn't
//! answer in time, a cache another panel holds, caching turned off — gets a
//! temporary DB exactly as before; selection never fails a connect that a
//! temporary DB would have served.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use metor_db::peer_cache::PeerCache;
use metor_db::remote::{Peer, identify};
use metor_db::storage::StorageConfig;
use metor_db::{AtomicTimestampExt as _, DB};
use metor_proto_wkt::{DbInfoResp, InstanceId};

use super::{DbRuntime, SessionStorage, StorageDirectory};

/// How long the startup probe waits for a peer to say who it is. Shorter
/// than the mirror's own handshake bound: a slow answer costs only the
/// cache, never the connection.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) type Selected = Result<(SessionStorage, Arc<DB>), metor_db::Error>;

/// The peer a cached session's DB belongs to. The session mirrors only it:
/// a second system would write its history into this one's cache, and the
/// same address answering with another id is a different database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CachedPeer {
    pub(crate) id: InstanceId,
    pub(crate) addr: SocketAddr,
}

/// Probe `addr` and open the session DB for whatever answered, on a thread
/// of its own so the caller's UI keeps drawing while it waits.
pub(crate) fn select_for_peer(
    addr: SocketAddr,
    config: StorageConfig,
) -> impl Future<Output = Selected> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let info = probe(addr);
        let _ = tx.send(SessionStorage::for_peer(addr, info.as_ref(), &config));
    });
    async move {
        rx.await
            .unwrap_or_else(|_| Err(std::io::Error::other("storage selection panicked").into()))
    }
}

/// What a metor-db at `addr` says about itself, or `None` when something
/// else answers (an fsw link) or nothing does in time.
fn probe(addr: SocketAddr) -> Option<DbInfoResp> {
    stellarator::run(|| async move {
        let answer = futures_lite::future::or(async { identify(addr).await.ok() }, async {
            stellarator::sleep(PROBE_TIMEOUT).await;
            None
        })
        .await;
        match answer {
            Some(Peer::Db(info)) => Some(info),
            Some(Peer::Fsw { .. }) | None => None,
        }
    })
}

/// The cache a mirror of this peer may keep: only a peer that names itself
/// and outlives its run. An ephemeral peer's id is new every run, so its
/// cache would never be reopened.
fn cache_key(info: &DbInfoResp) -> Option<InstanceId> {
    info.instance_id.filter(|_| !info.ephemeral)
}

impl SessionStorage {
    /// The DB for mirroring the peer that answered `info`: its cache when
    /// it has one this panel can claim, a temporary DB otherwise.
    ///
    /// A cached DB's head is raised to the peer's before anything reads
    /// it, so the temporal controller latches the session start at the
    /// peer's "now" rather than where the cache's old data ends.
    pub(crate) fn for_peer(
        addr: SocketAddr,
        info: Option<&DbInfoResp>,
        config: &StorageConfig,
    ) -> Selected {
        if let Some(id) = info.and_then(cache_key)
            && let Some(cache) = config.peer_cache()
        {
            let opened = Self::cached(&cache, CachedPeer { id, addr });
            let config = config.clone();
            std::thread::spawn(move || {
                config.prune_cache(&cache, Some(id));
            });
            if let Some((storage, db)) = opened {
                if let Some(head) = info.and_then(|info| info.last_updated) {
                    db.last_updated.update_max(head);
                }
                return Ok((storage, db));
            }
        }
        Self::create()
    }

    /// Claim and open `id`'s cache, or `None` to fall back to a temporary
    /// DB. Opening an existing DB spawns its components' persistence and
    /// lifecycle tasks, so it runs on an executor the session keeps.
    fn cached(cache: &PeerCache, peer: CachedPeer) -> Option<(Self, Arc<DB>)> {
        let id = peer.id;
        let mut cached = match cache.open(id) {
            Ok(Some(cached)) => cached,
            Ok(None) => {
                tracing::info!(%id, "peer cache is held by another panel; using a temporary db");
                return None;
            }
            Err(error) => {
                tracing::warn!(%id, %error, "could not open peer cache; using a temporary db");
                return None;
            }
        };
        let runtime = DbRuntime(Default::default());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        runtime
            .0
            .spawn(stellarator::util::CancelToken::new(), move || async move {
                let result = cached.open_db().map(Arc::new);
                let ok = result.is_ok();
                if tx.send((cached, result)).is_ok() && ok {
                    std::future::pending::<()>().await;
                }
            });
        let (cached, db) = match rx.recv() {
            Ok((cached, Ok(db))) => (cached, db),
            Ok((_, Err(error))) => {
                tracing::warn!(%id, ?error, "could not open cached db; using a temporary db");
                return None;
            }
            Err(_) => return None,
        };
        tracing::info!(%id, fresh = cached.fresh, path = %cached.path().display(), "mirroring into peer cache");
        Some((
            Self {
                runtime: Some(runtime),
                directory: StorageDirectory::Cached(cached, peer),
                source: None,
            },
            db,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 2240);

    fn info(id: Option<u8>, ephemeral: bool) -> DbInfoResp {
        DbInfoResp {
            protocol_version: 5,
            features: 0,
            command_ids: Vec::new(),
            namespace: None,
            commands: Vec::new(),
            last_updated: Some(metor_proto::types::Timestamp(1_000)),
            instance_id: id.map(|n| InstanceId([n; 16])),
            ephemeral,
        }
    }

    fn config(root: &std::path::Path) -> StorageConfig {
        StorageConfig {
            cache_dir: Some(root.to_owned()),
            ..Default::default()
        }
    }

    fn is_cached(storage: &SessionStorage) -> bool {
        matches!(storage.directory, StorageDirectory::Cached(..))
    }

    #[test]
    fn a_named_lasting_peer_gets_its_cache_and_reopens_it() {
        let root = tempfile::tempdir().unwrap();
        let peer = info(Some(1), false);
        let (storage, db) = SessionStorage::for_peer(ADDR, Some(&peer), &config(root.path())).unwrap();
        assert!(is_cached(&storage));
        assert!(matches!(
            storage.directory,
            StorageDirectory::Cached(_, CachedPeer { id, addr }) if id == InstanceId([1; 16]) && addr == ADDR
        ));
        let path = db.path.clone();
        assert!(path.starts_with(root.path().join(InstanceId([1; 16]).to_string())));
        assert_eq!(
            db.last_updated.latest(),
            metor_proto::types::Timestamp(1_000),
            "the head is the peer's"
        );
        storage.finish(&db, Ok(())).unwrap();
        assert!(path.exists(), "a cache outlives the session");
        drop(db);

        let (storage, db) = SessionStorage::for_peer(ADDR, Some(&peer), &config(root.path())).unwrap();
        assert!(is_cached(&storage));
        assert_eq!(db.path, path, "the same peer reopens the same db");
        storage.finish(&db, Ok(())).unwrap();
    }

    #[test]
    fn anything_that_cannot_name_a_cache_gets_a_temporary_db() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let cases = [
            ("ephemeral", Some(info(Some(1), true))),
            ("no instance id", Some(info(None, false))),
            ("fsw link or no answer", None),
        ];
        for (case, info) in cases {
            let (storage, db) = SessionStorage::for_peer(ADDR, info.as_ref(), &config).unwrap();
            assert!(storage.is_temporary(), "{case}");
            storage.finish(&db, Ok(())).unwrap();
        }
    }

    #[test]
    fn a_held_cache_falls_back_to_a_temporary_db() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let peer = info(Some(2), false);
        let (held, held_db) = SessionStorage::for_peer(ADDR, Some(&peer), &config).unwrap();
        assert!(is_cached(&held));
        let (storage, db) = SessionStorage::for_peer(ADDR, Some(&peer), &config).unwrap();
        assert!(
            storage.is_temporary(),
            "a second panel can't share the cache"
        );
        storage.finish(&db, Ok(())).unwrap();
        held.finish(&held_db, Ok(())).unwrap();
    }

    #[test]
    fn a_silent_peer_probes_as_uncacheable_within_the_timeout() {
        // The kernel completes the connection from the backlog, and nothing
        // ever answers: an old server that records GetDbInfo as telemetry.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let started = std::time::Instant::now();
        assert!(probe(addr).is_none());
        assert!(started.elapsed() < PROBE_TIMEOUT * 2);
    }

    #[test]
    fn a_db_server_probes_with_its_identity() {
        let dir = tempfile::tempdir().unwrap();
        let server_db = Arc::new(DB::create(dir.path().join("server")).unwrap());
        let id = server_db.instance_id();
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = metor_db::Server {
            listener,
            db: server_db,
        };
        let _server = stellarator::struc_con::stellar(move || server.run());
        let info = probe(addr).expect("a db answers");
        assert_eq!(info.instance_id, Some(id));
        assert!(!info.ephemeral);
    }
}
