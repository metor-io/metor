//! The fsw half of the unified remote client: identify what answered at an
//! address, and stream an fsw link into the local DB.
//!
//! A metor-db server and an fsw link server speak one wire protocol in two
//! modes. [`identify`] tells them apart with a single read: the client
//! probes with `GetDbInfo` — the one message both peers tolerate (a db
//! answers, an fsw drops node-protocol ids silently) — and the first packet
//! decides, because an fsw's identity push is the head of every
//! connection's replay and a db sends nothing unsolicited.
//!
//! [`fsw_stream`] then runs the fsw mode as two raced loops over the one
//! connection: ingest (the mirror's shape — every pushed packet through the
//! local packet handler) and a single command forwarder that tails the
//! advertised msg logs from the live edge. There is no supervisor here;
//! reconnecting is a plain loop at the call site around
//! `identify`/`fsw_stream`.

use std::net::SocketAddr;
use std::sync::Arc;

use metor_proto::types::{IntoLenPacket, LenPacket, Msg as _, OwnedPacket, PacketId, PacketTy};
use metor_proto_stellar::{Client, PacketSink, PacketStream};
use metor_proto_wkt::{DbInfoResp, GetDbInfo, LinkInfo};
use stellarator::io::{OwnedReader, OwnedWriter};
use stellarator::net::TcpStream;
use stellarator::sync::Mutex;
use tracing::warn;

use crate::{ConnState, DB, Error, PacketTx, msg_log_2::MsgLog};

use super::db::handshake_request;

/// What answered at the far end of one shared wire protocol.
pub enum Peer {
    /// Answered the probe: a metor-db server. The probe connection is
    /// dropped; callers hand off to [`RemoteDb`](super::RemoteDb), which
    /// dials and supervises itself.
    Db(DbInfoResp),
    /// Pushed its identity unprompted: an fsw link server. The connection
    /// is live and the announce replay is already streaming behind the
    /// identity — feed it to [`fsw_stream`].
    Fsw {
        info: LinkInfo,
        rx: PacketStream<OwnedReader<TcpStream>>,
        tx: PacketSink<OwnedWriter<TcpStream>>,
        /// The recycled receive buffer, mid-flight from the identity read.
        buf: Vec<u8>,
    },
}

/// Dial `addr` and let the first packet say what lives there. Bounded by
/// the shared handshake timeout; a peer that answers with neither identity
/// (or nothing) is an error naming what was expected.
pub async fn identify(addr: SocketAddr) -> Result<Peer, Error> {
    let Client { tx, mut rx, .. } = Client::connect(addr).await?;
    tx.send((&GetDbInfo).with_request_id(1)).await.0?;
    let pkt = handshake_request(rx.next_grow(vec![0u8; 64 * 1024])).await?;
    let OwnedPacket::Msg(m) = &pkt else {
        return Err(identity_err("a table packet"));
    };
    if m.id == LinkInfo::ID {
        let info: LinkInfo =
            postcard::from_bytes(&m.buf).map_err(|_| identity_err("a malformed LinkInfo"))?;
        let buf = pkt.into_buf().into_inner();
        Ok(Peer::Fsw { info, rx, tx, buf })
    } else if m.id == DbInfoResp::ID {
        let resp: DbInfoResp =
            postcard::from_bytes(&m.buf).map_err(|_| identity_err("a malformed DbInfoResp"))?;
        Ok(Peer::Db(resp))
    } else {
        Err(identity_err("an unknown first message"))
    }
}

fn identity_err(got: &str) -> Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("peer sent {got}; expected a DbInfoResp (metor-db) or LinkInfo (fsw link) identity"),
    )
    .into()
}

/// Stream an identified fsw link into `db` until the connection drops,
/// returning the error that ended it (the caller loops). Two raced loops
/// over the one socket: ingest every pushed packet through the local
/// packet handler — the same shapes a directly-attached producer sends —
/// and forward the advertised command set's msg logs up the link from
/// their live edge (nothing queued before this call is replayed).
pub async fn fsw_stream(
    info: LinkInfo,
    rx: PacketStream<OwnedReader<TcpStream>>,
    tx: PacketSink<OwnedWriter<TcpStream>>,
    buf: Vec<u8>,
    db: &Arc<DB>,
) -> Error {
    // The sink is shared the way the mirror already shares it (`PacketTx`
    // holds it behind this same lock); the ingest arms never reply to an
    // fsw stream, so the forwarder's sends are uncontended in practice.
    let tx = Arc::new(Mutex::new(tx));
    futures_lite::future::race(
        ingest(rx, buf, tx.clone(), db),
        forward(info.command_ids, tx, db),
    )
    .await
}

/// The mirror's ingest tail: every packet through `handle_packet`,
/// warn-and-continue on a bad one, return on a dead socket.
async fn ingest(
    mut rx: PacketStream<OwnedReader<TcpStream>>,
    mut buf: Vec<u8>,
    tx: Arc<Mutex<PacketSink<OwnedWriter<TcpStream>>>>,
    db: &Arc<DB>,
) -> Error {
    let mut conn = ConnState::default();
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
            warn!(?err, "failed to ingest fsw link packet");
        }
        resp_pkt = pkt_tx.pkt.expect("len pkt taken and not given back");
        buf = pkt.into_buf().into_inner();
    }
}

/// One forwarder for the whole advertised set: drain every log's WAL
/// reader — a fresh reader only ever sees records committed from now on,
/// so live-edge needs no cursor, and unlike the dial-in
/// `handle_msg_stream`'s wait→latest it never coalesces a burst (a
/// `Load`+`Start` pair both cross) — then park until any log pushes.
/// Sweep-then-wait makes lost wakes harmless: a push racing the wakeup is
/// caught by the next sweep, and a push while parked wakes the registered
/// waiter (the log wakes on `push`, before its persist task runs, which is
/// also why the WAL and not the persisted nodes is what a live tail reads).
async fn forward(
    ids: Vec<PacketId>,
    tx: Arc<Mutex<PacketSink<OwnedWriter<TcpStream>>>>,
    db: &Arc<DB>,
) -> Error {
    use crate::disruptor::Reader;
    use crate::msg_log_2::read_msg;

    let mut logs: Vec<(PacketId, MsgLog, Reader)> = Vec::new();
    for id in ids {
        let log = match db.with_state_mut(|s| s.get_or_insert_msg_log(id, &db.path).cloned()) {
            Ok(log) => log,
            Err(err) => return err,
        };
        let reader = log.wal_reader();
        logs.push((id, log, reader));
    }
    if logs.is_empty() {
        std::future::pending::<()>().await;
    }
    loop {
        for (id, _, reader) in &mut logs {
            while let Some(grant) = reader.try_next() {
                let mut buf: &[u8] = &grant;
                while let Some((rest, _ts, msg)) = read_msg(buf) {
                    buf = rest;
                    let mut pkt = LenPacket::msg(*id, msg.len());
                    pkt.extend_from_slice(msg);
                    let tx = tx.lock().await;
                    let (sent, _) = tx.send(pkt).await;
                    if let Err(err) = sent {
                        return err.into();
                    }
                }
            }
        }
        wait_any(logs.iter().map(|(_, log, _)| log)).await;
    }
}

/// Park until any of the logs' wait queues wakes — `futures_lite` has only
/// the binary race, so this is the n-ary one, polled in order.
async fn wait_any<'a>(logs: impl Iterator<Item = &'a MsgLog>) {
    use std::pin::Pin;
    use std::task::Poll;
    let mut waits: Vec<Pin<Box<dyn Future<Output = ()> + 'a>>> = logs
        .map(|log| Box::pin(log.wait()) as Pin<Box<dyn Future<Output = ()> + 'a>>)
        .collect();
    std::future::poll_fn(|cx| {
        for wait in &mut waits {
            if wait.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_proto_wkt::LINK_PROTOCOL_VERSION;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    const CMD_A: PacketId = [0x51, 0];
    const CMD_B: PacketId = [0x52, 0];
    const DATA: PacketId = [0x33, 7];

    fn identity(command_ids: Vec<PacketId>) -> Vec<u8> {
        (&LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids,
        })
            .into_len_packet()
            .inner
    }

    async fn wait_for(pred: impl Fn() -> bool, what: &str) {
        for _ in 0..200 {
            if pred() {
                return;
            }
            stellarator::sleep(Duration::from_millis(25)).await;
        }
        panic!("never saw {what}");
    }

    /// A hand-written fsw link server: accept one connection, push the
    /// identity and one self-describing data msg, then record every inbound
    /// msg. No metor-fsw-2 dependency — the wire shapes are the contract.
    fn fake_fsw(
        listener: stellarator::net::TcpListener,
        command_ids: Vec<PacketId>,
    ) -> Arc<StdMutex<Vec<(PacketId, Vec<u8>)>>> {
        let inbound: Arc<StdMutex<Vec<(PacketId, Vec<u8>)>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen = inbound.clone();
        stellarator::struc_con::stellar(move || async move {
            use stellarator::io::{AsyncWrite, SplitExt};
            let stream = listener.accept().await.expect("accept");
            let (rx, tx) = stream.split();
            tx.write_all(identity(command_ids)).await.0.expect("identity");
            let mut data = LenPacket::msg(DATA, 8);
            data.extend_from_slice(b"hello");
            tx.write_all(data.inner).await.0.expect("data");
            let mut packets = PacketStream::new(rx);
            let mut buf = vec![0u8; 1024];
            loop {
                let Ok(pkt) = packets.next_grow(buf).await else {
                    return;
                };
                // The real server's read hygiene: probes and other protocol
                // strays never reach the command queue.
                if let OwnedPacket::Msg(m) = &pkt
                    && !metor_proto_wkt::NODE_PROTOCOL_MESSAGES.contains(&m.id)
                {
                    seen.lock().unwrap().push((m.id, m.buf[..].to_vec()));
                }
                buf = pkt.into_buf().into_inner();
            }
        });
        inbound
    }

    #[stellarator::test]
    async fn identify_tells_an_fsw_link() {
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = fake_fsw(listener, vec![CMD_A, CMD_B]);
        let peer = identify(addr).await.expect("identify");
        let Peer::Fsw { info, .. } = peer else {
            panic!("expected an fsw peer");
        };
        assert_eq!(info.protocol_version, LINK_PROTOCOL_VERSION);
        assert_eq!(info.command_ids, vec![CMD_A, CMD_B]);
    }

    #[stellarator::test]
    async fn identify_tells_a_db_server() {
        let dir = tempfile::tempdir().unwrap();
        let server_db = Arc::new(crate::DB::create(dir.path().join("server")).unwrap());
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = crate::Server {
            listener,
            db: server_db,
        };
        stellarator::struc_con::stellar(move || server.run());
        let peer = identify(addr).await.expect("identify");
        assert!(matches!(peer, Peer::Db(_)), "expected a db peer");
    }

    #[stellarator::test]
    async fn identify_times_out_on_a_silent_peer() {
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept and hold the socket without ever writing.
        stellarator::struc_con::stellar(move || async move {
            let _stream = listener.accept().await;
            std::future::pending::<()>().await;
        });
        assert!(identify(addr).await.is_err(), "silence must not hang");
    }

    /// The full fsw round trip through one connection: the pushed stream
    /// lands in the local DB, commands queued before connect stay local,
    /// and commands pushed after connect reach the server — both advertised
    /// ids through the one forwarder.
    #[stellarator::test]
    async fn fsw_stream_ingests_and_forwards_live_edge() {
        use metor_proto::types::Timestamp;

        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::DB::create(dir.path().join("db")).unwrap());
        // Queued before connect: must never be replayed to the link.
        db.push_msg(Timestamp(1), CMD_A, b"stale").unwrap();

        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let inbound = fake_fsw(listener, vec![CMD_A, CMD_B]);

        let Peer::Fsw { info, rx, tx, buf } = identify(addr).await.expect("identify") else {
            panic!("expected an fsw peer");
        };
        let stream_db = db.clone();
        let _link = stellarator::spawn(async move {
            let err = fsw_stream(info, rx, tx, buf, &stream_db).await;
            tracing::info!(?err, "link ended");
        })
        .drop_guard();

        // The pushed data msg ingests into the local log.
        let ingested = db.clone();
        wait_for(
            move || {
                ingested
                    .with_state_mut(|s| s.get_or_insert_msg_log(DATA, &ingested.path).cloned())
                    .is_ok_and(|log| {
                        log.latest()
                            .and_then(|m| m.data().map(|d| d == b"hello"))
                            .unwrap_or(false)
                    })
            },
            "the pushed msg ingested",
        )
        .await;

        // Fresh commands on both advertised ids cross the link.
        db.push_msg(Timestamp(2), CMD_A, b"go").unwrap();
        db.push_msg(Timestamp(3), CMD_B, b"ack").unwrap();
        let seen = inbound.clone();
        wait_for(
            move || seen.lock().unwrap().len() >= 2,
            "both commands forwarded",
        )
        .await;
        let seen = inbound.lock().unwrap().clone();
        assert!(seen.contains(&(CMD_A, b"go".to_vec())), "{seen:?}");
        assert!(seen.contains(&(CMD_B, b"ack".to_vec())), "{seen:?}");
        assert!(
            !seen.iter().any(|(_, payload)| payload == b"stale"),
            "pre-connect commands must not replay: {seen:?}"
        );
    }
}
