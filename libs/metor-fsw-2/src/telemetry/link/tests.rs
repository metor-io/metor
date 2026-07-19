//! The link server socket-free (deterministic buffers, drop policy, queue
//! caps) and over one real loopback connection.

use metor_proto::types::PacketTy;
use metor_proto_wkt::{MsgMetadata, SetMsgMetadata};

use super::*;

fn server() -> LinkState {
    LinkState::bind("127.0.0.1:0".parse().unwrap()).expect("bind an ephemeral port")
}

fn msg_announce() -> Announce {
    Announce::Msg(SetMsgMetadata {
        id: [7, 7],
        metadata: MsgMetadata {
            name: "test".into(),
            schema: postcard_schema::schema::owned::OwnedNamedType::from(
                <u32 as postcard_schema::Schema>::SCHEMA,
            ),
            metadata: Default::default(),
        },
    })
}

/// Fan-out appends the identical bytes to every live connection, and a
/// connection over its pending cap misses whole batches alone.
#[test]
fn broadcast_fans_out_and_drops_over_cap() {
    let state = server();
    state.set_announces(&[]).ok();
    let a = state.push_test_conn();
    let b = state.push_test_conn();

    let batch: Vec<u8> = vec![0xAB; 600 << 10];
    state.broadcast(&batch);
    assert_eq!(a.pending_bytes(), batch);
    assert_eq!(b.pending_bytes(), batch);

    // `a`'s writer drained; `b` stalled. The next batch fits only `a`.
    a.pending.borrow_mut().clear();
    state.broadcast(&batch);
    assert_eq!(a.pending_bytes(), batch, "drained conn keeps receiving");
    assert_eq!(b.pending_bytes().len(), batch.len(), "stalled conn missed the batch");
    let stats = state.take_stats();
    assert_eq!(stats.conn_dropped, 1);
    assert_eq!(stats.accepted, 2);
}

/// A new connection's buffer starts with the announce replay, and a second
/// announce set is rejected rather than clobbering the replay.
#[test]
fn announce_blob_seeds_connections_once() {
    let state = server();
    let announce = msg_announce();
    let Announce::Msg(msg) = &announce else {
        unreachable!()
    };
    let expected = msg.into_len_packet().inner;
    state.set_announces(std::slice::from_ref(&announce)).ok();

    let conn = state.push_test_conn();
    assert_eq!(conn.pending_bytes(), expected);
    assert!(state.set_announces(&[]).is_err(), "second downlink rejected");
}

/// The inbound queue is bounded; overflow is counted, not grown.
#[test]
fn inbound_queue_caps_and_counts() {
    let mut state = server();
    for i in 0..300u16 {
        state.push_inbound([9, 9], &i.to_le_bytes());
    }
    let mut seen = 0;
    state.drain_inbound(|id, payload| {
        assert_eq!(id, [9, 9]);
        assert_eq!(payload.len(), 2);
        seen += 1;
    });
    assert_eq!(seen, 256);
    assert_eq!(state.take_stats().inbound_dropped, 44);
    let mut after = 0;
    state.drain_inbound(|_, _| after += 1);
    assert_eq!(after, 0, "drained empty");
}

/// Closed connections drop out of the set and the gauge.
#[test]
fn closed_connections_prune() {
    let state = server();
    state.set_announces(&[]).ok();
    let a = state.push_test_conn();
    let b = state.push_test_conn();
    assert_eq!(state.connections(), 2);
    b.close();
    assert_eq!(state.connections(), 1);
    state.broadcast(&[1, 2, 3]);
    assert_eq!(a.pending_bytes(), vec![1, 2, 3]);
    assert!(state.has_connections());
    a.close();
    assert!(!state.has_connections());
}

/// One real loopback connection: the announce replay arrives first, then
/// broadcast batches; the client's inbound `Msg` packets queue for the
/// uplink while `MsgStream` subscriptions are accepted and ignored.
#[cfg(not(miri))]
#[stellarator::test]
async fn loopback_replays_then_streams_and_reads_commands() {
    use metor_proto::types::IntoLenPacket;

    let mut state = server();
    let addr = state.local_addr();
    crate::SharedLifecycle::start(&mut state);
    state.set_announces(std::slice::from_ref(&msg_announce())).ok();

    let client = TcpStream::connect(addr).await.expect("connect");
    let (rx, tx) = client.split();
    let mut stream = PacketStream::new(rx);

    // The announce replay is the first thing on the wire.
    let pkt = stream.next_grow(vec![0u8; 256]).await.expect("announce");
    let OwnedPacket::Msg(m) = &pkt else {
        panic!("expected a Msg packet")
    };
    assert_eq!(m.id, SetMsgMetadata::ID);

    // The server sees the connection once its accept task has run.
    while state.connections() == 0 {
        stellarator::yield_now().await;
    }

    // A broadcast batch arrives verbatim after the replay.
    let mut batch = Vec::new();
    super::super::append_packet(&mut batch, PacketTy::Msg, [0x42, 0], b"hi");
    state.broadcast(&batch);
    let pkt = stream.next_grow(pkt.into_buf().into_inner()).await.expect("batch");
    let OwnedPacket::Msg(m) = &pkt else {
        panic!("expected the broadcast msg")
    };
    assert_eq!(m.id, [0x42, 0]);
    assert_eq!(&m.buf[..], b"hi");

    // Client → server: a subscription (ignored) and a command (queued).
    let sub = MsgStream { msg_id: [1, 1] }.into_len_packet().inner;
    tx.write_all(sub).await.0.expect("send subscription");
    let mut cmd = Vec::new();
    super::super::append_packet(&mut cmd, PacketTy::Msg, [0x51, 0], b"go");
    tx.write_all(cmd).await.0.expect("send command");

    let mut inbound: Vec<(PacketId, Vec<u8>)> = Vec::new();
    while inbound.is_empty() {
        stellarator::yield_now().await;
        state.drain_inbound(|id, payload| inbound.push((id, payload.to_vec())));
    }
    assert_eq!(inbound, vec![([0x51, 0], b"go".to_vec())]);

    crate::SharedLifecycle::shutdown(&mut state);
}
