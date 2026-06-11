use std::net::SocketAddr;

use metor_proto::types::{IntoLenPacket, Msg, OwnedPacket};
use metor_proto_stellar::Client;
use metor_proto_wkt::{
    DbInfoResp, DumpSchema, DumpSchemaResp, ErrorResponse, FetchNode, FetchNodeDone, GetDbInfo,
    GetNodeManifest, NODE_PROTOCOL_VERSION, NodeAck, NodeChunk, NodeManifestResp, OfferNode,
    OfferNodeResp, PushNodeDone,
};
use stellarator::sync::{Mutex, MutexGuard};
use tracing::warn;

use crate::seal::SealRecord;

use super::{
    BoxFuture, CHUNK_BYTES, NodeKey, NodeStaging, NodeStore, SealedChunk, SealedNode, StoreError,
};

/// This store owns its connection outright, so bulk streams can use a
/// fixed request id without colliding with anyone.
const BULK_REQ_ID: u8 = 251;

/// [`NodeStore`] backed by a peer metor-db over the wire protocol
/// (`GetNodeManifest`/`FetchNode`/`OfferNode`…). Maintains one dedicated
/// TCP connection, separate from any live-telemetry subscription, so bulk
/// node transfer never head-of-line-blocks streaming. The connection is
/// dialed lazily, handshaken via [`GetDbInfo`] (mandatory — older servers
/// record unknown requests as telemetry), and redialed after any
/// transport error.
pub struct PeerStore {
    addr: SocketAddr,
    client: Mutex<Option<Client>>,
}

impl PeerStore {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            client: Mutex::new(None),
        }
    }

    async fn lock_connected(&self) -> Result<MutexGuard<'_, Option<Client>>, StoreError> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            let mut client = Client::connect(self.addr).await.map_err(unavailable)?;
            let info: DbInfoResp = client.request(&GetDbInfo).await.map_err(unavailable)?;
            if info.protocol_version > NODE_PROTOCOL_VERSION {
                warn!(
                    peer = info.protocol_version,
                    ours = NODE_PROTOCOL_VERSION,
                    "peer speaks a newer node protocol"
                );
            }
            *guard = Some(client);
        }
        Ok(guard)
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

    fn contains<'a>(&'a self, key: NodeKey<'a>) -> BoxFuture<'a, Result<bool, StoreError>> {
        Box::pin(async move {
            let nodes = self.list(key.component_id, key.component_name).await?;
            Ok(nodes
                .iter()
                .any(|s| s.start_ts == key.start_ts && s.checksum == key.checksum))
        })
    }
}
