use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use metor_proto::schema::Schema;
use metor_proto::types::{ComponentId, IntoLenPacket, LenPacket, PacketTy, Timestamp};
use metor_proto_stellar::Client;
use metor_proto_wkt::{
    DbInfoResp, DumpMetadata, DumpMetadataResp, GetDbInfo, Stream,
    StreamBehavior,
};
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
    store: Arc<PeerStore>,
    hydrator: Hydrator,
    on_event: Option<EventFn>,
}

impl RemoteDb {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            store: Arc::new(PeerStore::new(addr)),
            hydrator: Hydrator::new(),
            on_event: None,
        }
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
                match mirror(&db, addr, &emit).await {
                    Ok(()) => emit(MirrorEvent::Disconnected),
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

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound a handshake request: old-protocol peers record unknown request
/// messages as telemetry and never reply, which would otherwise hang the
/// mirror forever.
async fn handshake_request<T>(
    fut: impl Future<Output = Result<T, metor_proto_stellar::Error>>,
) -> Result<T, Error> {
    futures_lite::future::or(async { fut.await.map_err(Error::from) }, async {
        stellarator::sleep(HANDSHAKE_TIMEOUT).await;
        Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into())
    })
    .await
}

/// One connection lifetime: handshake, metadata sync, then ingest the
/// real-time stream until the connection drops.
async fn mirror(db: &Arc<DB>, addr: SocketAddr, emit: &impl Fn(MirrorEvent)) -> Result<(), Error> {
    let mut client = Client::connect(addr).await?;
    let info: DbInfoResp = handshake_request(client.request(&GetDbInfo)).await?;
    info!(?addr, peer_version = info.protocol_version, "mirroring remote db");
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

    // From here the remote pushes VTableMsg + table packets — the same
    // shapes local producers send — so the local packet handler ingests
    // them verbatim. Replies (none are expected) go back over the same
    // socket.
    let Client { tx, mut rx, .. } = client;
    let tx = Arc::new(Mutex::new(tx));
    let mut conn = ConnState::default();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut resp_pkt = LenPacket::new(PacketTy::Msg, [0, 0], 1024 * 1024);
    loop {
        let pkt = rx.next_grow(buf).await?;
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

    fn event_log() -> (Arc<StdMutex<Vec<MirrorEvent>>>, impl Fn(MirrorEvent) + Send + Sync) {
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
        if seals.is_empty() {
            continue;
        }
        let inserted = db.with_state_mut(|state| {
            state.insert_component(component_id, ComponentSchema::from(schema), &db.path)
        });
        if let Err(err) = inserted {
            warn!(?err, ?component_id, "failed to create mirrored component");
            continue;
        }
        let Some(component) =
            db.with_state(|state| state.components.get(&component_id).cloned())
        else {
            continue;
        };
        let bounds = seals
            .iter()
            .fold(None::<(i64, i64)>, |acc, seal| match acc {
                Some((min, max)) => {
                    Some((min.min(seal.start_ts.0), max.max(seal.end_ts.0)))
                }
                None => Some((seal.start_ts.0, seal.end_ts.0)),
            });
        match component.time_series.merge_remote_spans(seals) {
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
