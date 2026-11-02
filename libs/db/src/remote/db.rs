use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use metor_proto::schema::Schema;
use metor_proto::types::{ComponentId, IntoLenPacket, LenPacket, PacketTy, Timestamp};
use metor_proto_stellar::{Client, PacketSink, PacketStream};
use metor_proto_wkt::{
    DbInfoResp, DumpMetadata, DumpMetadataResp, FEATURE_MSG_SYNC, GetDbInfo, InstanceId, Stream,
    StreamBehavior, SyncMsgs,
};
use stellarator::io::{OwnedReader, OwnedWriter};
use stellarator::net::TcpStream;
use stellarator::sync::Mutex;
use tracing::{info, warn};

use crate::{
    AtomicTimestampExt as _, ComponentSchema, ConnState, DB, Error, PacketTx, store::NodeStore,
    store::PeerStore,
};

use super::Hydrator;

/// Lifecycle notification from the mirror supervisor, so an embedding UI
/// can surface connection health. Plain values through a plain callback —
/// this crate stays free of any UI machinery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MirrorEvent {
    /// A connection attempt is starting (also fires before each reconnect).
    Connecting,
    /// The handshake completed; live telemetry is flowing.
    Connected,
    /// The peer closed the stream; the supervisor will reconnect.
    Disconnected,
    /// The attempt failed with an error; the supervisor will retry.
    Failed,
    /// A database other than the one [`RemoteDb::expect_instance`] named
    /// answered (`found` is its id, `None` from a server that doesn't say).
    /// Terminal: the supervisor stops, since writing that peer into the
    /// expected one's mirror would mix two histories.
    WrongPeer {
        expected: InstanceId,
        found: Option<InstanceId>,
    },
}

type EventFn = Arc<dyn Fn(MirrorEvent) + Send + Sync>;

/// Mirror of a long-running remote metor-db: a live-tail connection feeds
/// the local DB exactly like a directly-attached producer would, while a
/// separate bulk connection ([`PeerStore`]) serves on-demand hydration of
/// sealed history — live telemetry never queues behind a 32MB node fetch.
///
/// The live mirror leans on the existing wire protocol: the remote's
/// `Stream { RealTime }` pushes a `VTableMsg` + table packets per
/// component (discovering new components as they appear), and those are
/// the same packets the local ingest path already understands.
pub struct RemoteDb {
    addr: SocketAddr,
    expected: Option<InstanceId>,
    store: Arc<PeerStore>,
    hydrator: Hydrator,
    on_event: Option<EventFn>,
}

impl RemoteDb {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            expected: None,
            store: Arc::new(PeerStore::new(addr)),
            hydrator: Hydrator::new(),
            on_event: None,
        }
    }

    /// Mirror only the database `id` (when `Some`), checked on every
    /// connect: a mirror kept in that peer's cache must not take in
    /// whatever answers at the address after the peer was wiped or
    /// replaced. Another answer ends the mirror with
    /// [`MirrorEvent::WrongPeer`], and the bulk store refuses it too. Call
    /// before [`store`](Self::store), whose handle this replaces.
    pub fn expect_instance(mut self, id: Option<InstanceId>) -> Self {
        self.expected = id;
        self.store = Arc::new(PeerStore::new(self.addr).expect_instance(id));
        self
    }

    /// Observe supervisor lifecycle transitions. The callback fires from the
    /// mirror's runtime thread, so it must hand off to the observer's own
    /// scheduling rather than block.
    pub fn on_event(mut self, f: impl Fn(MirrorEvent) + Send + Sync + 'static) -> Self {
        self.on_event = Some(Arc::new(f));
        self
    }

    /// The handle render loops use to request remote-only ranges.
    pub fn hydrator(&self) -> Hydrator {
        self.hydrator.clone()
    }

    /// The bulk store, for offload/tiering against the same peer.
    pub fn store(&self) -> Arc<PeerStore> {
        self.store.clone()
    }

    /// Spawn the supervisor: the reconnecting live mirror plus the
    /// hydration worker. Tasks run until the runtime shuts down.
    pub fn spawn(self, db: Arc<DB>) {
        let Self {
            addr,
            expected,
            store,
            hydrator,
            on_event,
        } = self;
        let emit = move |event| {
            if let Some(f) = &on_event {
                f(event);
            }
        };
        stellarator::spawn(hydrator.clone().run(db.clone(), store.clone()));
        stellarator::spawn(seed_loop(db.clone(), store.clone()));
        stellarator::spawn(async move {
            const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
            const HEALTHY_CONNECTION: Duration = Duration::from_secs(30);
            let mut backoff = INITIAL_BACKOFF;
            loop {
                let connected_at = std::time::Instant::now();
                emit(MirrorEvent::Connecting);
                match mirror(&db, addr, expected, &emit).await {
                    Ok(event) => {
                        warn!(?addr, ?event, "a different database answered; mirror stopped");
                        emit(event);
                        return;
                    }
                    Err(err) if err.is_stream_closed() => {
                        info!(?addr, "remote db disconnected; reconnecting");
                        emit(MirrorEvent::Disconnected);
                    }
                    Err(err) => {
                        warn!(?err, ?addr, "remote db mirror failed; reconnecting");
                        emit(MirrorEvent::Failed);
                    }
                }
                if connected_at.elapsed() >= HEALTHY_CONNECTION {
                    backoff = INITIAL_BACKOFF;
                }
                stellarator::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        });
    }
}

pub(super) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound a handshake request: old-protocol peers record unknown request
/// messages as telemetry and never reply, which would otherwise hang the
/// mirror forever.
pub(super) async fn handshake_request<T>(
    fut: impl Future<Output = Result<T, metor_proto_stellar::Error>>,
) -> Result<T, Error> {
    futures_lite::future::or(async { fut.await.map_err(Error::from) }, async {
        stellarator::sleep(HANDSHAKE_TIMEOUT).await;
        Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into())
    })
    .await
}

/// One connection lifetime: handshake, metadata sync, then the real-time
/// stream — plus the message logs when the peer answers [`SyncMsgs`] —
/// ingested until the connection drops, raced against the forwarder for
/// the commands the peer advertised. Returns only with the error that
/// ended it, or with the [`MirrorEvent::WrongPeer`] that ends mirroring
/// before anything was ingested.
async fn mirror(
    db: &Arc<DB>,
    addr: SocketAddr,
    expected: Option<InstanceId>,
    emit: &impl Fn(MirrorEvent),
) -> Result<MirrorEvent, Error> {
    let mut client = Client::connect(addr).await?;
    let info: DbInfoResp = handshake_request(client.request(&GetDbInfo)).await?;
    if let Some(expected) = expected
        && info.instance_id != Some(expected)
    {
        return Ok(MirrorEvent::WrongPeer {
            expected,
            found: info.instance_id,
        });
    }
    info!(
        ?addr,
        peer_version = info.protocol_version,
        "mirroring remote db"
    );
    // Advertised again by this db, so its own clients' commands reach the
    // peer through the forwarder below.
    db.add_command_ids(&info.command_ids);
    db.add_commands(&info.commands);
    // Before the first live sample, the peer's head is the only sign of
    // where its data ends; seeded manifests stop at its last seal.
    if let Some(head) = info.last_updated {
        db.last_updated.update_max(head);
    }
    emit(MirrorEvent::Connected);

    let metadata: DumpMetadataResp = handshake_request(client.request(&DumpMetadata)).await?;
    db.with_state_mut(|state| {
        for component_metadata in metadata.component_metadata {
            if let Err(err) = state.set_component_metadata(component_metadata, &db.path) {
                warn!(?err, "failed to mirror component metadata");
            }
        }
    });

    let stream = Stream {
        behavior: StreamBehavior::RealTime,
        id: fastrand::u64(..),
    };
    client.send((&stream).with_request_id(1)).await.0?;
    if info.features & FEATURE_MSG_SYNC != 0 {
        client.send((&SyncMsgs).with_request_id(2)).await.0?;
    }

    // From here the remote pushes VTableMsg + table packets — the same
    // shapes local producers send — so the local packet handler ingests
    // them verbatim. Replies (none are expected) go back over the same
    // socket, as do the commands the peer advertised.
    let Client { tx, rx, .. } = client;
    let tx = Arc::new(Mutex::new(tx));
    Err(futures_lite::future::race(
        ingest(rx, tx.clone(), db),
        super::forward(info.command_ids, info.commands, tx, db),
    )
    .await)
}

/// Every pushed packet through the local packet handler, warn-and-continue
/// on a bad one, returning the error that ended the connection.
async fn ingest(
    mut rx: PacketStream<OwnedReader<TcpStream>>,
    tx: Arc<Mutex<PacketSink<OwnedWriter<TcpStream>>>>,
    db: &Arc<DB>,
) -> Error {
    let mut conn = ConnState::mirror();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut resp_pkt = LenPacket::new(PacketTy::Msg, [0, 0], 1024 * 1024);
    loop {
        let pkt = match rx.next_grow(buf).await {
            Ok(pkt) => pkt,
            Err(err) => return err.into(),
        };
        let mut pkt_tx = PacketTx {
            req_id: pkt.req_id(),
            tx: tx.clone(),
            pkt: Some(resp_pkt),
        };
        if let Err(err) = crate::handle_packet(&pkt, db, &mut pkt_tx, &mut conn).await {
            warn!(?err, "failed to ingest mirrored packet");
        }
        resp_pkt = pkt_tx.pkt.expect("len pkt taken and not given back");
        buf = pkt.into_buf().into_inner();
    }
}

/// Periodically import the peer's sealed-node manifests, on the bulk
/// store connection so a year of GetNodeManifest round trips never delays
/// live ingest. The first pass per peer connect imports pre-connect
/// history; later passes pick up spans the peer sealed since — hidden
/// components (derived LoD series) especially, which never ride the live
/// stream. The merge is idempotent, so re-seeding is cheap when nothing
/// changed.
async fn seed_loop(db: Arc<DB>, store: Arc<PeerStore>) {
    const RESEED_INTERVAL: Duration = Duration::from_secs(60);
    loop {
        match store.dump_schema().await {
            Ok(resp) => seed_manifests(&db, &store, resp.schemas).await,
            Err(err) => {
                warn!(?err, "failed to dump peer schemas for manifest seeding");
            }
        }
        stellarator::sleep(RESEED_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn event_log() -> (
        Arc<StdMutex<Vec<MirrorEvent>>>,
        impl Fn(MirrorEvent) + Send + Sync,
    ) {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let sink = log.clone();
        (log, move |event| sink.lock().unwrap().push(event))
    }

    async fn wait_for(log: &StdMutex<Vec<MirrorEvent>>, wanted: MirrorEvent) {
        for _ in 0..100 {
            if log.lock().unwrap().contains(&wanted) {
                return;
            }
            stellarator::sleep(Duration::from_millis(50)).await;
        }
        panic!("never saw {wanted:?}; log: {:?}", log.lock().unwrap());
    }

    #[stellarator::test]
    async fn unreachable_peer_emits_connecting_then_failed() {
        let (log, sink) = event_log();
        // A port nothing listens on: bind, note the addr, drop the listener.
        let addr = {
            let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::DB::create(dir.path().join("db")).unwrap());
        RemoteDb::new(addr).on_event(sink).spawn(db);
        wait_for(&log, MirrorEvent::Failed).await;
        assert_eq!(log.lock().unwrap().first(), Some(&MirrorEvent::Connecting));
    }

    #[stellarator::test]
    async fn live_peer_emits_connected() {
        let (log, sink) = event_log();
        let dir = tempfile::tempdir().unwrap();
        let server_db = Arc::new(crate::DB::create(dir.path().join("server")).unwrap());
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = crate::Server {
            listener,
            db: server_db,
        };
        stellarator::struc_con::stellar(move || server.run());
        let mirror_db = Arc::new(crate::DB::create(dir.path().join("mirror")).unwrap());
        RemoteDb::new(addr).on_event(sink).spawn(mirror_db);
        wait_for(&log, MirrorEvent::Connected).await;
        assert_eq!(log.lock().unwrap().first(), Some(&MirrorEvent::Connecting));
    }

    /// A mirror pinned to one database stops for good when another answers
    /// at its address, and takes nothing in from it.
    #[stellarator::test]
    async fn a_different_database_at_the_address_stops_the_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let server_db = Arc::new(crate::DB::create(dir.path().join("server")).unwrap());
        let found = server_db.instance_id();
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = crate::Server {
            listener,
            db: server_db.clone(),
        };
        stellarator::struc_con::stellar(move || server.run());

        let expected = InstanceId([9; 16]);
        assert_ne!(found, expected);
        let (log, sink) = event_log();
        let mirror_db = Arc::new(crate::DB::create(dir.path().join("mirror")).unwrap());
        RemoteDb::new(addr)
            .expect_instance(Some(expected))
            .on_event(sink)
            .spawn(mirror_db);
        let wrong = MirrorEvent::WrongPeer {
            expected,
            found: Some(found),
        };
        wait_for(&log, wrong).await;
        // Longer than the first reconnect backoff: no further attempt.
        stellarator::sleep(Duration::from_millis(600)).await;
        assert_eq!(*log.lock().unwrap(), [MirrorEvent::Connecting, wrong]);
        assert!(matches!(
            PeerStore::new(addr)
                .expect_instance(Some(expected))
                .dump_schema()
                .await,
            Err(crate::store::StoreError::Unavailable(_))
        ));

        // The right id mirrors as before.
        let (log, sink) = event_log();
        let mirror_db = Arc::new(crate::DB::create(dir.path().join("mirror2")).unwrap());
        RemoteDb::new(addr)
            .expect_instance(Some(found))
            .on_event(sink)
            .spawn(mirror_db);
        wait_for(&log, MirrorEvent::Connected).await;
    }

    /// What the mirror's live ingest writes is mirrored data, while the
    /// peer's own ingest of the same component stays local.
    #[stellarator::test]
    async fn mirrored_components_ingest_as_mirrored() {
        use metor_proto::types::PrimType;
        use metor_proto::vtable::builder::{
            component, raw_field, raw_table, schema, timestamp, vtable,
        };

        const ID: metor_proto::types::PacketId = [9, 2];
        let dir = tempfile::tempdir().unwrap();
        let server_db = Arc::new(crate::DB::create(dir.path().join("server")).unwrap());
        server_db
            .insert_vtable(metor_proto_wkt::VTableMsg {
                id: ID,
                vtable: vtable([raw_field(
                    8,
                    8,
                    timestamp(raw_table(0, 8), schema(PrimType::F64, &[1], component("a"))),
                )]),
            })
            .unwrap();
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = crate::Server {
            listener,
            db: server_db.clone(),
        };
        stellarator::struc_con::stellar(move || server.run());
        let mirror_db = Arc::new(crate::DB::create(dir.path().join("mirror")).unwrap());
        RemoteDb::new(addr).spawn(mirror_db.clone());

        let id = ComponentId::new("a");
        let source = |db: &crate::DB| {
            db.with_state(|s| s.get_component(id).map(|c| c.time_series.is_mirrored()))
        };
        for ts in 1i64.. {
            let mut table = ts.to_le_bytes().to_vec();
            table.extend_from_slice(&1.5f64.to_le_bytes());
            server_db.ingest_table(ID, &table).unwrap();
            if source(&mirror_db) == Some(true) {
                break;
            }
            assert!(ts < 100, "mirror never ingested the live stream");
            stellarator::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(source(&server_db), Some(false));
    }

    /// Polls `check` every 20 ms until it holds, failing after ten seconds.
    async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..500 {
            if check() {
                return;
            }
            stellarator::sleep(Duration::from_millis(20)).await;
        }
        panic!("never saw {what}");
    }

    /// What a session opened: its db, whether the cache was fresh, and its
    /// hydrator.
    type Opened = (Arc<DB>, bool, Hydrator);

    /// One mirror session into `id`'s peer cache, on a runtime of its own
    /// so cancelling it ends the session: its tasks stop, and the cache's
    /// lock is released.
    fn cached_session(
        cache: crate::peer_cache::PeerCache,
        id: metor_proto_wkt::InstanceId,
        addr: SocketAddr,
        cancel: stellarator::util::CancelToken,
    ) -> (
        std::sync::mpsc::Receiver<Opened>,
        stellarator::struc_con::Thread<Option<()>>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = stellarator::struc_con::ThreadBuilder::default()
            .cancel_token(cancel)
            .stellar(move || async move {
                let mut cached = cache.open(id).unwrap().expect("cache is free");
                let db = Arc::new(cached.open_db().unwrap());
                let remote = RemoteDb::new(addr);
                // Handed out before the mirror starts, so the caller sees
                // the db as the session opened it.
                tx.send((db.clone(), cached.fresh, remote.hydrator()))
                    .unwrap();
                remote.spawn(db);
                let _held = cached;
                std::future::pending::<()>().await
            });
        (rx, thread)
    }

    async fn recv<T>(rx: &std::sync::mpsc::Receiver<T>) -> T {
        loop {
            match rx.try_recv() {
                Ok(value) => return value,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    stellarator::sleep(Duration::from_millis(10)).await
                }
                Err(err) => panic!("session thread ended: {err}"),
            }
        }
    }

    /// A mirror kept in the peer cache picks up where it left off: what the
    /// first session held and the manifests it seeded are there the moment
    /// the second opens, its live data lands in new nodes, and what the
    /// peer wrote between the sessions hydrates from the peer.
    #[stellarator::test]
    async fn a_reopened_peer_cache_resumes_and_fetches_the_gap() {
        use crate::manifest::SpanState;
        use metor_proto::types::PrimType;
        use metor_proto::vtable::builder::{
            component, raw_field, raw_table, schema, timestamp, vtable,
        };
        use stellarator::struc_con::Joinable as _;

        const ID: metor_proto::types::PacketId = [9, 3];
        let dir = tempfile::tempdir().unwrap();
        let peer = Arc::new(crate::DB::create(dir.path().join("peer")).unwrap());
        peer.insert_vtable(metor_proto_wkt::VTableMsg {
            id: ID,
            vtable: vtable([raw_field(
                8,
                8,
                timestamp(raw_table(0, 8), schema(PrimType::F64, &[1], component("a"))),
            )]),
        })
        .unwrap();
        let write = |ts: i64| {
            let mut table = ts.to_le_bytes().to_vec();
            table.extend_from_slice(&(ts as f64).to_le_bytes());
            peer.ingest_table(ID, &table).unwrap();
        };
        let a = ComponentId::new("a");
        let series = |db: &DB| db.with_state(|s| s.get_component(a).map(|c| c.time_series.clone()));
        let latest =
            |db: &DB| series(db).and_then(|ts| ts.latest().map(|sample| sample.timestamp().0));
        // Small peer nodes, so history seals (and so can be seeded and
        // fetched) a hundred samples at a time.
        write(0);
        series(&peer)
            .unwrap()
            .set_max_node_age(Duration::from_micros(100));
        for ts in 1..250 {
            write(ts);
            if ts % 50 == 0 {
                stellarator::sleep(Duration::from_millis(1)).await;
            }
        }
        let sealed = |db: &DB| series(db).map_or(0, |ts| ts.manifest().spans.len());
        eventually("the peer sealing its history", || sealed(&peer) >= 2).await;

        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = crate::Server {
            listener,
            db: peer.clone(),
        };
        let _server = stellarator::struc_con::stellar(move || server.run());
        let cache = crate::peer_cache::PeerCache::new(dir.path().join("peers"));
        let id = peer.instance_id();

        // Session one: seed the peer's manifests and watch live data.
        let cancel = stellarator::util::CancelToken::new();
        let (rx, session) = cached_session(cache.clone(), id, addr, cancel.clone());
        let (mirror, fresh, _) = recv(&rx).await;
        assert!(fresh);
        let mut ts = 250;
        eventually("the mirror seeding manifests and ingesting live", || {
            write(ts);
            ts += 1;
            latest(&mirror).is_some_and(|l| l >= 250) && sealed(&mirror) >= 2
        })
        .await;
        let seeded = sealed(&mirror);
        cancel.cancel();
        let _ = session.join().await;
        mirror.flush().unwrap();
        let held = latest(&mirror).unwrap();
        drop(mirror);

        // Between sessions the peer keeps writing.
        let gap_start = ts;
        for t in gap_start..gap_start + 700 {
            write(t);
            if t % 50 == 0 {
                stellarator::sleep(Duration::from_millis(1)).await;
            }
        }
        let resume = gap_start + 700;
        eventually("the peer sealing the gap", || {
            series(&peer)
                .unwrap()
                .manifest()
                .spans
                .iter()
                .any(|s| s.seal.start_ts.0 > gap_start + 100 && s.seal.end_ts.0 < resume - 100)
        })
        .await;
        let gap = *series(&peer)
            .unwrap()
            .manifest()
            .spans
            .iter()
            .find(|s| s.seal.start_ts.0 > gap_start + 100 && s.seal.end_ts.0 < resume - 100)
            .unwrap();

        // Session two: the same directory, with everything already there.
        let cancel = stellarator::util::CancelToken::new();
        let (rx, session) = cached_session(cache.clone(), id, addr, cancel.clone());
        let (mirror, fresh, hydrator) = recv(&rx).await;
        assert!(!fresh, "the cache is reopened");
        assert_eq!(latest(&mirror), Some(held), "what session one held");
        assert!(sealed(&mirror) >= seeded, "and the manifests it seeded");

        let mut ts = resume;
        eventually("live data in a new node", || {
            write(ts);
            ts += 1;
            series(&mirror)
                .and_then(|s| {
                    s.list
                        .iter()
                        .next()
                        .and_then(|n| n.timestamps().first().copied())
                })
                .is_some_and(|head| head.0 > held)
        })
        .await;

        let gap_state = || {
            series(&mirror).and_then(|s| s.manifest().span(gap.cover_start).map(|span| span.state))
        };
        eventually("the gap seeded as remote-only", || {
            gap_state() == Some(SpanState::RemoteOnly)
        })
        .await;
        let range = gap.seal.start_ts..Timestamp(gap.seal.end_ts.0 + 1);
        eventually("the gap hydrating from the peer", || {
            hydrator.request(a, range.clone(), super::super::HydratePriority::Visible);
            series(&mirror).is_some_and(|s| {
                s.manifest()
                    .span(gap.cover_start)
                    .is_some_and(|span| span.state == SpanState::Resident)
            })
        })
        .await;
        cancel.cancel();
        let _ = session.join().await;
    }
}

/// Import the peer's sealed-node manifest for every component it knows,
/// so history that never rides the live stream shows up as fetchable
/// remote-only coverage. Components are created on demand (a seeded
/// component has no live writer, so the merge cannot conflict with
/// ingest); ones with no sealed history are skipped rather than created
/// empty. Per-component failures are logged and skipped — one bad
/// component must not hide the rest of the archive.
async fn seed_manifests(
    db: &Arc<DB>,
    store: &Arc<PeerStore>,
    schemas: HashMap<ComponentId, Schema<Vec<u64>>>,
) {
    let mut components = 0usize;
    let mut spans = 0usize;
    for (component_id, schema) in schemas {
        let name = db
            .with_state(|state| {
                state
                    .get_component_metadata(component_id)
                    .map(|m| m.name.clone())
            })
            .unwrap_or_default();
        let seals = match store.list(component_id, &name).await {
            Ok(seals) => seals,
            Err(err) => {
                warn!(?err, ?component_id, "failed to list remote sealed nodes");
                continue;
            }
        };
        // An empty listing still matters to a component seeded before: the
        // peer dropped every seal it had.
        let existing = db.with_state(|state| state.components.get(&component_id).cloned());
        if seals.is_empty() {
            if let Some(component) = existing
                && let Err(err) = component.time_series.sync_remote_spans(seals)
            {
                warn!(?err, ?component_id, "failed to sync remote spans");
            }
            continue;
        }
        let inserted = db.with_state_mut(|state| {
            state.insert_component(component_id, ComponentSchema::from(schema), &db.path)
        });
        if let Err(err) = inserted {
            warn!(?err, ?component_id, "failed to create mirrored component");
            continue;
        }
        let Some(component) = db.with_state(|state| state.components.get(&component_id).cloned())
        else {
            continue;
        };
        let bounds = seals
            .iter()
            .fold(None::<(i64, i64)>, |acc, seal| match acc {
                Some((min, max)) => Some((min.min(seal.start_ts.0), max.max(seal.end_ts.0))),
                None => Some((seal.start_ts.0, seal.end_ts.0)),
            });
        match component.time_series.sync_remote_spans(seals) {
            Ok(0) => {}
            Ok(added) => {
                if let Some((min, max)) = bounds {
                    db.earliest_timestamp.update_min(Timestamp(min));
                    db.last_updated.update_max(Timestamp(max));
                }
                components += 1;
                spans += added;
            }
            Err(err) => {
                warn!(?err, ?component_id, "failed to merge remote spans");
            }
        }
    }
    if spans > 0 {
        info!(components, spans, "seeded remote manifests");
    }
}
