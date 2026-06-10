use std::{net::SocketAddr, sync::Arc, time::Duration};

use metor_proto::types::{IntoLenPacket, LenPacket, PacketTy};
use metor_proto_stellar::Client;
use metor_proto_wkt::{
    DbInfoResp, DumpMetadata, DumpMetadataResp, GetDbInfo, Stream, StreamBehavior,
};
use stellarator::sync::Mutex;
use tracing::{info, warn};

use crate::{ConnState, DB, Error, PacketTx, store::PeerStore};

use super::Hydrator;

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
}

impl RemoteDb {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            store: Arc::new(PeerStore::new(addr)),
            hydrator: Hydrator::new(),
        }
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
        } = self;
        stellarator::spawn(hydrator.clone().run(db.clone(), store));
        stellarator::spawn(async move {
            const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
            const HEALTHY_CONNECTION: Duration = Duration::from_secs(30);
            let mut backoff = INITIAL_BACKOFF;
            loop {
                let connected_at = std::time::Instant::now();
                match mirror(&db, addr).await {
                    Ok(()) => {}
                    Err(err) if err.is_stream_closed() => {
                        info!(?addr, "remote db disconnected; reconnecting");
                    }
                    Err(err) => {
                        warn!(?err, ?addr, "remote db mirror failed; reconnecting");
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
async fn mirror(db: &Arc<DB>, addr: SocketAddr) -> Result<(), Error> {
    let mut client = Client::connect(addr).await?;
    let info: DbInfoResp = handshake_request(client.request(&GetDbInfo)).await?;
    info!(?addr, peer_version = info.protocol_version, "mirroring remote db");

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
