use metor_fsw_3_ring::{NoWake, RingBuffer};
use metor_proto::types::{PacketId, PacketTy};
use zerocopy::IntoBytes;

use crate::DefCx;
use crate::io::test::{TestIo, TestPort, cap_with_ops};
use crate::io::tests::allocations;
use crate::io::{Buf, Cap, Endpoint, Op, RawHandle};
use crate::port::{DynInputs, DynOutputs, Input};
use crate::record::Payload;
use crate::system::{InputBinding, OutputBinding, PortDef};
use crate::tests::utils::{Imu, ring};

use super::batch::Batcher;
use super::conns::{ConnsMem, ops_for};
use super::subscribe::Routes;
use super::*;

#[test]
fn test_status_changes_on_counters_only() {
    let status = LinkStatus {
        connections: 1,
        ..Default::default()
    };
    let restamped = LinkStatus {
        timestamp: Timestamp(9),
        ..status
    };
    assert!(!status.changed(&restamped));
    for moved in [
        LinkStatus {
            connections: 2,
            ..status
        },
        LinkStatus {
            bytes_out: 1,
            ..status
        },
        LinkStatus {
            batches_dropped: 1,
            ..status
        },
        LinkStatus {
            inbound_dropped: 1,
            ..status
        },
    ] {
        assert!(status.changed(&moved), "{moved:?}");
    }
}

fn ops(params: serde_json::Value) -> Result<u16, DefError> {
    link_ops(&DefCx {
        params: &params,
        ..DefCx::empty()
    })
}

#[test]
fn test_link_ops_follow_the_slots() {
    let budget = |slots| Ok(ops_for(slots) as u16);
    assert_eq!(ops(serde_json::Value::Null), budget(8), "the default slots");
    assert_eq!(ops(serde_json::json!({ "max_connections": 2 })), budget(2));
    let dialing = serde_json::json!({ "max_connections": 4, "dialing": true });
    assert_eq!(ops(dialing), budget(1), "a dialer has one slot");
}

#[test]
fn test_link_ops_reject_bad_params() {
    let zero = ops(serde_json::json!({ "max_connections": 0 }));
    assert!(matches!(zero, Err(DefError::Params { .. })), "{zero:?}");
    let huge = ops(serde_json::json!({ "max_connections": 1 << 20 }));
    assert!(matches!(&huge, Err(DefError::Params { message }) if message.contains("more ops")));
}

fn conn_cap_check(params: serde_json::Value, ports: &[PortDef]) -> Result<(), DefError> {
    let cx = DefCx {
        params: &params,
        ..DefCx::empty()
    };
    check_conn_cap(&cx, ports)
}

/// A port of `max_len`-byte records.
fn port(name: &str, max_len: usize) -> PortDef {
    PortDef {
        name: name.to_string().into(),
        max_len,
        ..Input::<Imu>::def(&DefCx::empty(), "imu")
    }
}

#[test]
fn test_conn_cap_fits_each_port() {
    let cap = |conn_cap: usize| serde_json::json!({ "conn_cap": conn_cap });
    let ports = [port("small", 16), port("image", 2 << 20)];
    assert_eq!(conn_cap_check(cap((2 << 20) + 8), &ports), Ok(()));
    assert_eq!(conn_cap_check(cap(1), &[]), Ok(()), "no port, any cap");
    let refused = DefError::PortExceedsConnCap {
        port: "image".into(),
        max_len: 2 << 20,
        conn_cap: (2 << 20) + 7,
    };
    assert_eq!(
        conn_cap_check(cap((2 << 20) + 7), &ports),
        Err(refused.clone())
    );
    assert!(
        refused
            .to_string()
            .ends_with("a `conn_cap` of 3145728 fits them"),
        "{refused}"
    );
    let default = conn_cap_check(serde_json::Value::Null, &ports);
    assert!(
        matches!(default, Err(DefError::PortExceedsConnCap { conn_cap, .. }) if conn_cap == 1 << 20)
    );
    assert!(wire::fits(usize::MAX - 8, usize::MAX));
    assert!(
        !wire::fits(usize::MAX - 7, usize::MAX),
        "overflow does not fit"
    );
}

const CAP: usize = 4096;
const STREAM: RawHandle = RawHandle::new(10, 1);
const LISTENER: RawHandle = RawHandle::new(11, 1);

/// One link's test IO and the bytes waiting on its end of the stream.
struct End {
    ep: Cap<Endpoint>,
    io: TestIo,
    inbox: Vec<u8>,
    opened: bool,
}

impl End {
    fn new() -> Self {
        let (ep, io) = cap_with_ops::<Endpoint>(ops_for(1) as u16);
        Self {
            ep,
            io,
            inbox: Vec::new(),
            opened: false,
        }
    }

    /// Makes one connection's buffers and a batch buffer, as a link's make
    /// does.
    fn make(&self, dialing: bool) -> (ConnsMem, Buf) {
        let config = ConnsConfig {
            slots: 1,
            conn_cap: CAP,
            dialing,
            write_timeout: Duration::from_secs(5),
        };
        self.io
            .make(|init| Ok((ConnsMem::make(init, config)?, init.alloc(CAP)?)))
    }
}

/// Answers one end's ops: the stream op opens the one stream, a send moves
/// bytes to the other end's inbox, and a recv takes from this end's.
fn serve(port: &mut TestPort, opened: &mut bool, inbox: &mut Vec<u8>, peer: &mut Vec<u8>) {
    let mut requests = port.requests();
    while let Some(mut r) = requests.next() {
        match r.op() {
            Op::Listen => r.done(Ok(LISTENER.get() as usize)),
            Op::Accept | Op::Connect if !*opened => {
                *opened = true;
                r.done(Ok(STREAM.get() as usize));
            }
            Op::Recv if !inbox.is_empty() => {
                let n = inbox.len().min(r.buf().len());
                r.buf()[..n].copy_from_slice(&inbox[..n]);
                inbox.drain(..n);
                r.done(Ok(n));
            }
            Op::Send => {
                peer.extend_from_slice(r.buf());
                let n = r.buf().len();
                r.done(Ok(n));
            }
            _ => {}
        }
    }
}

fn turn(a: &mut End, b: &mut End) {
    serve(a.io.turn(), &mut a.opened, &mut a.inbox, &mut b.inbox);
    serve(b.io.turn(), &mut b.opened, &mut b.inbox, &mut a.inbox);
}

/// A publishing link over one `Imu` port, and a dialing subscriber whose
/// one output takes the same port's records, run one turn at a time.
#[test]
fn test_records_cross_from_publish_to_subscribe() {
    let def = Input::<Imu>::def(&DefCx::empty(), "plant.imu");
    let (source, sink): (RingBuffer, RingBuffer) = (ring::<Imu>(8, 1), ring::<Imu>(8, 1));
    let view = source.view(NoWake).expect("a free slot");
    let mut inputs = DynInputs::new(vec![InputBinding {
        def: def.clone(),
        views: vec![view],
    }]);
    let writer = sink.writer(NoWake).expect("a free writer");
    let binding = OutputBinding { def, writer };
    let mut outputs = DynOutputs::new(vec![binding], &std::sync::Arc::default());
    let mut received = sink.view(NoWake).expect("a free slot");

    let (mut publisher, mut subscriber) = (End::new(), End::new());
    let defs: Vec<_> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
    let (announce, ids) = wire::announce(&defs, Some("ns"), "pub");
    let (mem, scratch) = publisher.make(false);
    let mut published = Conns::new(mem, &publisher.ep, &announce, 0).expect("fits");
    let mut batcher = Batcher::new(ids, scratch);
    let mut routes = Routes::of(&mut outputs);
    let tables = routes.ids(PacketTy::Table);
    let (mem, _) = subscriber.make(true);
    let mut subscribed = Conns::new(mem, &subscriber.ep, &[], tables.len()).expect("fits");

    let mut writer = source.writer(NoWake).expect("a free writer");
    let (mut missing, mut dropped): (Vec<PacketId>, u64) = (Vec::new(), 0);
    let (mut last, mut received_count, mut allocated) = (None, 0, 0);
    for cycle in 0..1_000 {
        let _ = writer.try_write(Imu::new(cycle, cycle as f64).as_bytes());
        turn(&mut publisher, &mut subscriber);
        let mut step = || {
            published.poll(|_, _| {});
            if let Some(batch) = batcher.fill(&mut inputs) {
                published.broadcast(batch);
            }
            subscribed.poll(|state, body| {
                if let Some((ty, id, bytes)) = state.route(&tables, body, |t| missing.push(t)) {
                    dropped += u64::from(routes.deliver(ty, id, bytes));
                }
            });
            for record in received.drain() {
                let imu = record.ok().and_then(|r| Imu::decode(r).ok());
                let sample = imu.expect("an imu").sample;
                let next = last.map(|l: f64| l + 1.0);
                assert!(next.is_none_or(|n| n == sample), "{sample} after {last:?}");
                (last, received_count) = (Some(sample), received_count + 1);
            }
            received.settle();
        };
        // The first cycles open the connections, which logs nothing but may
        // grow the harness's queues.
        match cycle < 20 {
            true => step(),
            false => allocated += allocations(step),
        }
    }
    assert!(received_count > 990, "{received_count}");
    assert_eq!(allocated, 0, "a running pair allocates nothing");
    assert!(missing.is_empty(), "the announce names the table");
    assert_eq!(dropped, 0);
    assert_eq!(published.stats().connections, 1);
    assert_eq!(subscribed.stats().connections, 1);
}
