//! The link server socket-free (deterministic buffers, drop policy, queue
//! caps) and over one real loopback connection.

use metor_proto::types::PacketTy;
use metor_proto_wkt::{GetDbInfo, MsgMetadata, SetMsgMetadata};

use super::*;

fn server() -> LinkState {
    LinkState::bind("127.0.0.1:0".parse().unwrap()).expect("bind an ephemeral port")
}

/// The identity packet a freshly-announced server seeds, for prefix
/// assertions.
fn identity_bytes(command_ids: Vec<PacketId>) -> Vec<u8> {
    (&LinkInfo {
        protocol_version: LINK_PROTOCOL_VERSION,
        features: 0,
        command_ids,
    })
        .into_len_packet()
        .inner
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

    let seeded = a.pending_bytes().len();
    let batch: Vec<u8> = vec![0xAB; 600 << 10];
    state.broadcast(&batch);
    assert_eq!(a.pending_bytes()[seeded..], batch);
    assert_eq!(b.pending_bytes()[seeded..], batch);

    // `a`'s writer drained; `b` stalled. The next batch fits only `a`.
    a.pending.borrow_mut().clear();
    state.broadcast(&batch);
    assert_eq!(a.pending_bytes(), batch, "drained conn keeps receiving");
    assert_eq!(
        b.pending_bytes().len(),
        seeded + batch.len(),
        "stalled conn missed the batch"
    );
    let stats = state.take_stats();
    assert_eq!(stats.conn_dropped, 1);
    assert_eq!(stats.accepted, 2);
}

/// A new connection's buffer starts with the identity packet (carrying the
/// advertised command set), then the announce replay; a second announce set
/// is rejected rather than clobbering the replay.
#[test]
fn announce_blob_seeds_connections_once() {
    let state = server();
    state
        .add_uplink_msgs(&[[9, 9], [8, 8], [9, 9]])
        .expect("advertise before the freeze");
    let announce = msg_announce();
    let Announce::Msg(msg) = &announce else {
        unreachable!()
    };
    let mut expected = identity_bytes(vec![[9, 9], [8, 8]]); // deduped, in order
    expected.extend_from_slice(&msg.into_len_packet().inner);
    state.set_announces(std::slice::from_ref(&announce)).ok();

    let conn = state.push_test_conn();
    assert_eq!(conn.pending_bytes(), expected);
    assert!(state.set_announces(&[]).is_err(), "second downlink rejected");
    assert!(
        state.add_uplink_msgs(&[[1, 1]]).is_err(),
        "advertising after the freeze is a config defect"
    );
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

/// Stray node/link protocol messages — a client's `GetDbInfo` probe — are
/// dropped silently on the read side: nothing queues and nothing counts.
#[cfg(not(miri))]
#[stellarator::test]
async fn probe_messages_never_reach_the_inbound_queue() {
    use metor_proto::types::IntoLenPacket;
    use stellarator::io::AsyncWrite;

    let mut state = server();
    let addr = state.local_addr();
    crate::SharedLifecycle::start(&mut state);
    state.set_announces(&[]).ok();

    let client = TcpStream::connect(addr).await.expect("connect");
    let (rx, tx) = client.split();
    // Drain the identity packet so the writer half is exercised too.
    let mut stream = PacketStream::new(rx);
    let pkt = stream.next_grow(vec![0u8; 256]).await.expect("identity");
    let OwnedPacket::Msg(m) = &pkt else {
        panic!("expected the identity push")
    };
    assert_eq!(m.id, LinkInfo::ID);

    // A probe, then a real command; only the command may queue.
    tx.write_all((&GetDbInfo).into_len_packet().inner)
        .await
        .0
        .expect("send probe");
    let mut cmd = Vec::new();
    super::super::append_packet(&mut cmd, PacketTy::Msg, [0x51, 0], b"go");
    tx.write_all(cmd).await.0.expect("send command");

    let mut inbound: Vec<PacketId> = Vec::new();
    while inbound.is_empty() {
        stellarator::yield_now().await;
        state.drain_inbound(|id, _| inbound.push(id));
    }
    assert_eq!(inbound, vec![[0x51, 0]], "the probe was dropped silently");
    assert_eq!(state.take_stats().inbound_dropped, 0);

    crate::SharedLifecycle::shutdown(&mut state);
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
    let seeded = a.pending_bytes().len();
    state.broadcast(&[1, 2, 3]);
    assert_eq!(a.pending_bytes()[seeded..], [1, 2, 3]);
    assert_ne!(state.connections(), 0);
    a.close();
    assert_eq!(state.connections(), 0);
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

    // The identity packet leads, then the announce replay.
    let pkt = stream.next_grow(vec![0u8; 256]).await.expect("identity");
    let OwnedPacket::Msg(m) = &pkt else {
        panic!("expected a Msg packet")
    };
    assert_eq!(m.id, LinkInfo::ID);
    let info: LinkInfo = postcard::from_bytes(&m.buf).expect("LinkInfo decodes");
    assert_eq!(info.protocol_version, LINK_PROTOCOL_VERSION);
    let pkt = stream.next_grow(pkt.into_buf().into_inner()).await.expect("announce");
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

/// Retained snapshot records ride every new connection's seed, after the
/// identity + announces, and an update replaces the slot in place.
#[test]
fn retained_records_replay_to_new_connections() {
    let state = server();
    state.set_retained_slots(2);
    state.set_announces(&[]).ok();
    let identity = identity_bytes(vec![]);

    // Nothing retained yet: the seed is just the identity.
    let bare = state.push_test_conn();
    assert_eq!(bare.pending_bytes(), identity);

    state.retain(0, b"AAAA");
    state.retain(1, b"BB");
    let seeded = state.push_test_conn();
    let mut expected = identity.clone();
    expected.extend_from_slice(b"AAAA");
    expected.extend_from_slice(b"BB");
    assert_eq!(seeded.pending_bytes(), expected, "slot order preserved");

    // A fresh record replaces its slot; earlier connections are untouched
    // (they already ingested the live broadcast).
    state.retain(0, b"A2");
    let updated = state.push_test_conn();
    let mut expected = identity;
    expected.extend_from_slice(b"A2");
    expected.extend_from_slice(b"BB");
    assert_eq!(updated.pending_bytes(), expected);
}
