//! The fsw half of the unified remote client: stream an identified fsw
//! link into the local DB.
//!
//! A metor-db server and an fsw link server speak one wire protocol in two
//! modes; [`identify`](metor_proto_stellar::identify) tells them apart
//! with a single read.
//!
//! [`fsw_stream`] then runs the fsw mode as two raced loops over the one
//! connection: ingest (the mirror's shape — every pushed packet through the
//! local packet handler) and a single [`forward`](super::forward) that
//! tails the caller's command logs from the live edge. There is no supervisor here;
//! reconnecting is a plain loop at the call site around
//! `identify`/`fsw_stream`.

use std::sync::Arc;

use metor_proto::types::{LenPacket, PacketId, PacketTy};
use metor_proto_stellar::{PacketSink, PacketStream};
use stellarator::io::{OwnedReader, OwnedWriter};
use stellarator::net::TcpStream;
use stellarator::sync::Mutex;
use tracing::warn;

use crate::{ConnState, DB, Error, PacketTx};

/// Stream an identified fsw link into `db` until the connection drops,
/// returning the error that ended it (the caller loops). Two raced loops
/// over the one socket: ingest every pushed packet through the local
/// packet handler — the same shapes a directly-attached producer sends —
/// and forward `command_ids`' msg logs up the link from their live edge
/// (nothing queued before this call is replayed).
pub async fn fsw_stream(
    command_ids: Vec<PacketId>,
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
        super::forward(command_ids, tx, db),
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

#[cfg(test)]
mod tests {
    use super::*;
    use metor_proto::types::{IntoLenPacket, OwnedPacket};
    use metor_proto_stellar::{Peer, identify};
    use metor_proto_wkt::{LINK_PROTOCOL_VERSION, LinkInfo};
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
            namespace: None,
            link: String::new(),
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
            tx.write_all(identity(command_ids))
                .await
                .0
                .expect("identity");
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
            let err = fsw_stream(info.command_ids, rx, tx, buf, &stream_db).await;
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

    /// The forwarded set is the caller's argument, not the link's
    /// advertisement: a narrowed set sends only its own ids.
    #[stellarator::test]
    async fn fsw_stream_forwards_only_the_given_set() {
        use metor_proto::types::Timestamp;

        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::DB::create(dir.path().join("db")).unwrap());
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let inbound = fake_fsw(listener, vec![CMD_A, CMD_B]);

        let Peer::Fsw { rx, tx, buf, .. } = identify(addr).await.expect("identify") else {
            panic!("expected an fsw peer");
        };
        let stream_db = db.clone();
        let _link = stellarator::spawn(async move {
            fsw_stream(vec![CMD_A], rx, tx, buf, &stream_db).await;
        })
        .drop_guard();

        // Wait for the link's own push to ingest: the forwarder's readers
        // are live by then, and only records after them are forwarded.
        let ingested = db.clone();
        wait_for(
            move || {
                ingested
                    .with_state_mut(|s| s.get_or_insert_msg_log(DATA, &ingested.path).cloned())
                    .is_ok_and(|log| log.latest().is_some())
            },
            "the link's push",
        )
        .await;

        db.push_msg(Timestamp(2), CMD_B, b"dropped").unwrap();
        db.push_msg(Timestamp(3), CMD_A, b"go").unwrap();
        let seen = inbound.clone();
        wait_for(
            move || !seen.lock().unwrap().is_empty(),
            "the forwarded command",
        )
        .await;
        stellarator::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            inbound.lock().unwrap().clone(),
            vec![(CMD_A, b"go".to_vec())]
        );
    }
}
