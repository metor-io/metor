use std::net::SocketAddr;
use std::time::Duration;

use metor_proto::types::LenPacket;
use stellarator::io::AsyncRead as _;

use super::*;

#[test]
fn inbound_slots_reuse_payload_capacity() {
    let (tx, rx) = thingbuf::mpsc::with_recycle(1, InboundRecycle);
    {
        let mut msg = tx.try_send_ref().unwrap();
        msg.payload.extend_from_slice(&[0x55; RECV_BUF]);
    }
    let capacity = {
        let msg = rx.try_recv_ref().unwrap();
        msg.payload.capacity()
    };
    let msg = tx.try_send_ref().unwrap();
    assert!(msg.payload.is_empty());
    assert_eq!(msg.payload.capacity(), capacity);
}

#[test]
fn outbound_queue_enforces_the_byte_cap_and_reuses_storage() {
    let queue: ArcBBQueue<BoxedSlice, AtomicCoord, MaiNotSpsc> =
        ArcBBQueue::new_with_storage(BoxedSlice::new(PENDING_CAP + 1));
    let tx = queue.stream_producer();
    let rx = queue.stream_consumer();
    let queued = AtomicUsize::new(0);

    assert!(enqueue_bytes(&tx, &queued, &[0x55; PENDING_CAP]));
    assert!(!enqueue_bytes(&tx, &queued, &[0x55]));

    let grant = rx.read().unwrap();
    assert_eq!(grant.len(), PENDING_CAP);
    grant.release(PENDING_CAP);
    queued.fetch_sub(PENDING_CAP, Relaxed);
    assert!(rx.read().is_err());
    assert!(enqueue_bytes(&tx, &queued, &[0xAA; PENDING_CAP]));

    // Reusing a completely consumed ring may wrap once, but bbqueue
    // exposes the two regions without allocating either one.
    let first = rx.read().unwrap();
    let first_len = first.len();
    assert_eq!(&*first, &[0xAA; 1]);
    first.release(first_len);
    let second = rx.read().unwrap();
    assert_eq!(first_len + second.len(), PENDING_CAP);
    assert!(second.iter().all(|byte| *byte == 0xAA));
}

#[test]
fn link_replays_fans_out_and_ingests_without_per_message_buffers() {
    stellarator::run(|| async {
        let mut link = LinkState::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let addr = link.local_addr;
        crate::SharedLifecycle::start(&mut link);

        // This connection is accepted before the replay exists and parks.
        let first = TcpStream::connect(addr).await.unwrap();
        let command_id = [0x12, 0x34];
        assert!(link.add_uplink_msgs(&[command_id]).is_ok());
        assert!(link.set_announces(&[]).is_ok());
        link.set_retained_slots(1);

        let info = LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids: vec![command_id],
        }
        .into_len_packet()
        .inner;
        let replay = vec![0; info.len()];
        let (res, replay) = first.read_exact(replay).await;
        res.unwrap();
        assert_eq!(replay, info);

        // Retention and live fan-out travel in one recycled cycle slot.
        let mut retained = b"retained".to_vec();
        link.retain(0, &mut retained);
        let mut live = b"live".to_vec();
        link.broadcast_buffer(&mut live);
        link.flush();

        let live_read = vec![0; 4];
        let (res, live_read) = first.read_exact(live_read).await;
        res.unwrap();
        assert_eq!(live_read, b"live");

        // A late joiner receives schemas first and the actor-owned newest
        // retained bytes second.
        let second = TcpStream::connect(addr).await.unwrap();
        let mut expected = info.clone();
        expected.extend_from_slice(b"retained");
        let replay = vec![0; expected.len()];
        let (res, replay) = second.read_exact(replay).await;
        res.unwrap();
        assert_eq!(replay, expected);

        let mut msg = LenPacket::msg(command_id, 3);
        msg.extend_from_slice(b"cmd");
        first.write_all(msg.inner).await.0.unwrap();
        let mut received = Vec::new();
        for _ in 0..8 {
            stellarator::sleep(Duration::from_millis(1)).await;
            link.drain_inbound(|id, payload| received.push((id, payload.to_vec())));
            if !received.is_empty() {
                break;
            }
        }
        assert_eq!(received, vec![(command_id, b"cmd".to_vec())]);

        crate::SharedLifecycle::shutdown(&mut link);
    });
}
