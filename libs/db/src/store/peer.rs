use std::net::SocketAddr;

use metor_proto::types::{IntoLenPacket, Msg, OwnedPacket};
use metor_proto_stellar::Client;
use metor_proto_wkt::{
    DbInfoResp, DumpSchema, DumpSchemaResp, ErrorResponse, FEATURE_RANGE_FETCH, FetchNode,
    FetchNodeDone, GetDbInfo, GetNodeManifest, GetTimeSeries, InstanceId, NODE_PROTOCOL_VERSION,
    NodeAck, NodeChunk, NodeManifestResp, OfferNode, OfferNodeResp, PushNodeDone,
};
use stellarator::sync::{Mutex, MutexGuard};
use tracing::warn;

use crate::seal::SealRecord;

use super::{
    BoxFuture, CHUNK_BYTES, NodeKey, NodeStaging, NodeStore, SealedChunk, SealedNode, SeriesReply,
    StoreError,
};

/// Most range fetches one batch pipelines; also the request ids it uses.
pub const MAX_BATCH: usize = 64;

/// Bound on one batch's round trip, so a wedged peer fails the batch and
/// the next one redials instead of parking every waiting read.
const BATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// This store owns its connection outright, so bulk streams can use a
/// fixed request id without colliding with anyone.
const BULK_REQ_ID: u8 = 251;

/// Same policy as `remote::db::HANDSHAKE_TIMEOUT` — the dial + handshake
/// must be bounded because it runs while holding the client mutex.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Race `fut` against a timeout, surfacing the loss as an io error.
async fn bounded<T>(
    timeout: std::time::Duration,
    fut: impl Future<Output = T>,
) -> Result<T, std::io::Error> {
    futures_lite::future::or(async { Ok(fut.await) }, async {
        stellarator::sleep(timeout).await;
        Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
    })
    .await
}

/// [`NodeStore`] backed by a peer metor-db over the wire protocol
/// (`GetNodeManifest`/`FetchNode`/`OfferNode`…). Maintains one dedicated
/// TCP connection, separate from any live-telemetry subscription, so bulk
/// node transfer never head-of-line-blocks streaming. The connection is
/// dialed lazily, handshaken via [`GetDbInfo`] (mandatory — older servers
/// record unknown requests as telemetry), and redialed after any
/// transport error.
pub struct PeerStore {
    addr: SocketAddr,
    /// The database this store must be talking to, when the caller knows
    /// it (see [`crate::remote::RemoteDb::expect_instance`]); any other
    /// answer at `addr` is refused rather than fetched from or listed.
    expected: Option<InstanceId>,
    client: Mutex<Option<Client>>,
    /// A second connection, for range fetches only. Bulk transfers hold
    /// [`Self::client`] for a whole node, which can take seconds; a scrub's
    /// point reads must not queue behind one. Requests on it are pipelined
    /// in batches (see [`NodeStore::time_series`]).
    ranges: Mutex<Option<RangeConn>>,
}

/// The range-fetch connection, and whether the peer at its far end
/// answers range fetches at all.
struct RangeConn {
    client: Client,
    supported: bool,
}

impl PeerStore {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            expected: None,
            client: Mutex::new(None),
            ranges: Mutex::new(None),
        }
    }

    /// Refuse to talk to any database at `addr` but `id`.
    pub fn expect_instance(mut self, id: Option<InstanceId>) -> Self {
        self.expected = id;
        self
    }

    async fn lock_connected(&self) -> Result<MutexGuard<'_, Option<Client>>, StoreError> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            *guard = Some(self.dial().await?.0);
        }
        Ok(guard)
    }

    /// Connect and handshake. Bounded like `remote::db::handshake_request`:
    /// old-protocol peers record unknown requests as telemetry and never
    /// reply, which would otherwise park this future while it holds a
    /// connection mutex — wedging every store consumer forever.
    async fn dial(&self) -> Result<(Client, DbInfoResp), StoreError> {
        let mut client = bounded(HANDSHAKE_TIMEOUT, Client::connect(self.addr))
            .await
            .map_err(unavailable)?
            .map_err(unavailable)?;
        let info: DbInfoResp = bounded(HANDSHAKE_TIMEOUT, client.request(&GetDbInfo))
            .await
            .map_err(unavailable)?
            .map_err(unavailable)?;
        if let Some(expected) = self.expected
            && info.instance_id != Some(expected)
        {
            return Err(StoreError::Unavailable(format!(
                "{} is no longer database {expected}",
                self.addr
            )));
        }
        if info.protocol_version > NODE_PROTOCOL_VERSION {
            warn!(
                peer = info.protocol_version,
                ours = NODE_PROTOCOL_VERSION,
                "peer speaks a newer node protocol"
            );
        }
        Ok((client, info))
    }

    /// Send `requests` down the range connection back to back, then read
    /// their replies, matched by request id. One round trip for the batch,
    /// however many it holds.
    async fn pipeline(
        client: &mut Client,
        requests: &[GetTimeSeries],
    ) -> Result<Vec<Result<SeriesReply, StoreError>>, StoreError> {
        for (i, request) in requests.iter().enumerate() {
            let id = (i + 1) as u8;
            client
                .send(request.with_request_id(id))
                .await
                .0
                .map_err(unavailable)?;
        }
        let mut replies: Vec<Option<Result<SeriesReply, StoreError>>> =
            (0..requests.len()).map(|_| None).collect();
        let mut pending = requests.len();
        while pending > 0 {
            let pkt = client
                .rx
                .next_grow(vec![0u8; 1024])
                .await
                .map_err(unavailable)?;
            let Some(slot) = (pkt.req_id() as usize)
                .checked_sub(1)
                .and_then(|i| replies.get_mut(i))
                .filter(|slot| slot.is_none())
            else {
                continue;
            };
            *slot = Some(match pkt {
                OwnedPacket::TimeSeries(series) => Ok(series),
                OwnedPacket::Msg(m) if m.id == ErrorResponse::ID => {
                    match m.parse::<ErrorResponse>() {
                        Ok(err) => Err(StoreError::Other(err.description)),
                        Err(err) => Err(StoreError::Other(err.to_string())),
                    }
                }
                _ => Err(StoreError::Other(
                    "unexpected reply to a range fetch".into(),
                )),
            });
            pending -= 1;
        }
        Ok(replies
            .into_iter()
            .map(|r| r.expect("every reply arrived"))
            .collect())
    }
}

fn unavailable(err: impl std::fmt::Display) -> StoreError {
    StoreError::Unavailable(err.to_string())
}

/// Poison the connection so the next call redials, passing `err` through.
fn poison(guard: &mut Option<Client>, err: StoreError) -> StoreError {
    *guard = None;
    err
}

/// Map a request error: a server-side [`ErrorResponse`] leaves the
/// connection healthy; anything else (transport, parse) poisons it so the
/// next call redials.
fn request_err(guard: &mut Option<Client>, err: metor_proto_stellar::Error) -> StoreError {
    match err {
        metor_proto_stellar::Error::Response(resp) => StoreError::Other(resp.description),
        err => poison(guard, unavailable(err)),
    }
}

impl PeerStore {
    /// Snapshot of every component the peer knows. Drives manifest
    /// seeding; inherent rather than on [`NodeStore`] because only a peer
    /// database can answer it.
    pub async fn dump_schema(&self) -> Result<DumpSchemaResp, StoreError> {
        let mut guard = self.lock_connected().await?;
        let client = guard.as_mut().expect("just connected");
        match client.request(&DumpSchema).await {
            Ok(resp) => Ok(resp),
            Err(err) => Err(request_err(&mut guard, err)),
        }
    }
}

impl NodeStore for PeerStore {
    fn put<'a>(
        &'a self,
        key: NodeKey<'a>,
        node: SealedNode<'a>,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut guard = self.lock_connected().await?;
            let client = guard.as_mut().expect("just connected");
            let offer = OfferNode {
                component_id: key.component_id,
                component_name: key.component_name.to_string(),
                schema: key.schema.clone(),
                seal: node.seal,
            };
            let resp: OfferNodeResp = match client.request(&offer).await {
                Ok(resp) => resp,
                Err(err) => return Err(request_err(&mut guard, err)),
            };
            if resp.already_have {
                return Ok(());
            }
            if !resp.accept {
                return Err(StoreError::Other("peer rejected the offer".to_string()));
            }
            let client = guard.as_mut().expect("just connected");
            for chunk in node.chunks(CHUNK_BYTES) {
                let msg = NodeChunk {
                    component_id: key.component_id,
                    start_ts: key.start_ts,
                    file: chunk.file,
                    offset: chunk.offset,
                    payload: chunk.bytes.to_vec(),
                };
                if let Err(err) = client.send((&msg).with_request_id(BULK_REQ_ID)).await.0 {
                    return Err(poison(&mut guard, unavailable(err)));
                }
            }
            let done = PushNodeDone {
                component_id: key.component_id,
                start_ts: key.start_ts,
                checksum: key.checksum,
            };
            let ack: NodeAck = match client.request(&done).await {
                Ok(ack) => ack,
                Err(err) => return Err(request_err(&mut guard, err)),
            };
            if ack.durable {
                Ok(())
            } else {
                Err(StoreError::Other(
                    "peer failed to persist the node".to_string(),
                ))
            }
        })
    }

    fn get<'a>(
        &'a self,
        key: NodeKey<'a>,
        dst: &'a NodeStaging,
    ) -> BoxFuture<'a, Result<SealRecord, StoreError>> {
        Box::pin(async move {
            let mut guard = self.lock_connected().await?;
            let client = guard.as_mut().expect("just connected");
            let req = FetchNode {
                component_id: key.component_id,
                start_ts: key.start_ts,
                chunk_bytes: CHUNK_BYTES as u32,
            };
            if let Err(err) = client.send((&req).with_request_id(BULK_REQ_ID)).await.0 {
                return Err(poison(&mut guard, unavailable(err)));
            }
            let mut buf = vec![0u8; CHUNK_BYTES + 1024];
            loop {
                let pkt = match client.rx.next_grow(buf).await {
                    Ok(pkt) => pkt,
                    Err(err) => return Err(poison(&mut guard, unavailable(err))),
                };
                let mut finished: Option<SealRecord> = None;
                match &pkt {
                    OwnedPacket::Msg(m) if m.id == NodeChunk::ID => {
                        let chunk: NodeChunk = match m.parse() {
                            Ok(chunk) => chunk,
                            Err(err) => return Err(poison(&mut guard, unavailable(err))),
                        };
                        let append = dst.append(&SealedChunk {
                            file: chunk.file,
                            offset: chunk.offset,
                            bytes: &chunk.payload,
                        });
                        if let Err(err) = append {
                            // The stream is mid-flight; drop the
                            // connection rather than resync.
                            return Err(poison(&mut guard, StoreError::Other(err.to_string())));
                        }
                    }
                    OwnedPacket::Msg(m) if m.id == FetchNodeDone::ID => {
                        let done: FetchNodeDone = match m.parse() {
                            Ok(done) => done,
                            Err(err) => return Err(poison(&mut guard, unavailable(err))),
                        };
                        finished = Some(done.seal);
                    }
                    OwnedPacket::Msg(m) if m.id == ErrorResponse::ID => {
                        return Err(StoreError::NotFound);
                    }
                    _ => {}
                }
                buf = pkt.into_buf().into_inner();
                if let Some(seal) = finished {
                    return Ok(seal);
                }
            }
        })
    }

    fn list<'a>(
        &'a self,
        component_id: metor_proto::types::ComponentId,
        _component_name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SealRecord>, StoreError>> {
        Box::pin(async move {
            let mut guard = self.lock_connected().await?;
            let client = guard.as_mut().expect("just connected");
            match client.request(&GetNodeManifest { component_id }).await {
                Ok(NodeManifestResp { nodes, .. }) => Ok(nodes),
                Err(err) => Err(request_err(&mut guard, err)),
            }
        })
    }

    /// Pipelined on the range connection, at most [`MAX_BATCH`] at a time:
    /// the server answers a connection's requests in order and each reply
    /// is small, so a batch costs one round trip. A transport error or a
    /// timeout fails the batch and drops the connection.
    fn time_series<'a>(
        &'a self,
        requests: &'a [GetTimeSeries],
    ) -> BoxFuture<'a, Result<Vec<Result<SeriesReply, StoreError>>, StoreError>> {
        Box::pin(async move {
            if requests.len() > MAX_BATCH {
                return Err(StoreError::Other(format!(
                    "a batch holds at most {MAX_BATCH} range fetches"
                )));
            }
            let mut guard = self.ranges.lock().await;
            if guard.is_none() {
                let (client, info) = self.dial().await?;
                let supported =
                    info.protocol_version >= 7 && info.features & FEATURE_RANGE_FETCH != 0;
                *guard = Some(RangeConn { client, supported });
            }
            let conn = guard.as_mut().expect("just connected");
            if !conn.supported {
                return Err(StoreError::Unsupported);
            }
            match bounded(BATCH_TIMEOUT, Self::pipeline(&mut conn.client, requests)).await {
                Ok(Ok(replies)) => Ok(replies),
                Ok(Err(err)) => {
                    *guard = None;
                    Err(err)
                }
                Err(err) => {
                    *guard = None;
                    Err(unavailable(err))
                }
            }
        })
    }

    fn contains<'a>(&'a self, key: NodeKey<'a>) -> BoxFuture<'a, Result<bool, StoreError>> {
        Box::pin(async move {
            let nodes = self.list(key.component_id, key.component_name).await?;
            Ok(nodes
                .iter()
                .any(|s| s.start_ts == key.start_ts && s.checksum == key.checksum))
        })
    }
}
