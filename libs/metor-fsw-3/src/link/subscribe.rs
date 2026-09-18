//! `Subscribe`: the records a target is sent, written onto its rings.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use metor_proto::types::{OwnedPacket, PacketId, Timestamp};
use schemars::JsonSchema;
use serde::Deserialize;
use stellarator::sync::WaitQueue;

use crate::coordinator::ParamError;
use crate::log::Log;
use crate::port::{DynOutputs, Output};
use crate::record::{Bytes, RecordSchema};
use crate::{Stop, system};

use super::conn::{Connections, Packet};
use super::transport::{Endpoint, Incoming, Transport, incoming};
use super::wire;
use super::{LinkStatus, pending_cap};

/// Packets a link holds for its ports before it drops one.
const INBOUND_CAP: usize = 256;

fn inbound_cap() -> usize {
    INBOUND_CAP
}

/// What a `Subscribe` is configured with.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubscribeParams {
    pub transport: Transport,
    /// The target's telemetry namespace, which its identity carries.
    #[serde(default)]
    pub namespace: Option<String>,
    /// This link's name, so a peer can tell one of a target's links from another.
    #[serde(default)]
    pub link: String,
    #[serde(default = "pending_cap")]
    pub pending_cap: usize,
    #[serde(default = "inbound_cap")]
    pub inbound_cap: usize,
}

/// Writes every message its peers send onto the port that carries it.
pub struct Subscribe {
    endpoint: Option<Endpoint>,
    namespace: Option<String>,
    link: String,
    pending_cap: usize,
    inbound_cap: usize,
}

impl Subscribe {
    /// Binds the transport, so a taken address fails the build.
    pub fn new(params: SubscribeParams) -> Result<Self, ParamError> {
        let endpoint = params.transport.bind()?;
        super::record_bound(&params.link, &endpoint);
        Ok(Self {
            endpoint: Some(endpoint),
            namespace: params.namespace,
            link: params.link,
            pending_cap: params.pending_cap,
            inbound_cap: params.inbound_cap,
        })
    }
}

#[system]
impl Subscribe {
    /// Tells each peer what it may send, then routes what arrives by its id.
    async fn run(
        &mut self,
        outputs: &mut DynOutputs,
        link_status: &mut Output<LinkStatus>,
        log: &mut Log,
        stop: Stop,
    ) {
        let (mut ports, max_len) = routes(outputs, log);
        let ids: Vec<PacketId> = ports.iter().map(|(id, _)| *id).collect();
        // PANIC Safety: `run` is called once, and the endpoint is the bind's.
        let endpoint = self.endpoint.take().expect("one run per instance");
        if let Some(addr) = endpoint.local_addr() {
            log.info(format!("listening on {addr}"));
        }
        let listens = endpoint.listens();
        let seed = match listens {
            true => wire::link_info(ids.clone(), self.namespace.as_deref(), &self.link),
            false => Vec::new(),
        };
        let inbox = Rc::new(Inbox::new(ids, self.inbound_cap, max_len));
        let mut conns = Connections::new(
            endpoint.slots(),
            self.pending_cap,
            max_len + wire::PACKET_OVERHEAD,
        );
        let (incoming, _source) = incoming(endpoint, stop.clone());
        incoming.want();
        let mut dropped = 0;
        let mut reported = LinkStatus::new(Timestamp(0), Default::default(), 0);
        loop {
            let event = next(&incoming, &inbox, &conns, &stop).await;
            match event {
                Event::Connected(stream) => {
                    let inbox = inbox.clone();
                    conns.open(stream, seed.clone(), move |packet| inbox.accept(packet));
                }
                Event::Inbound => dropped += deliver(&inbox, &mut ports),
                Event::Closed => {}
                Event::Stop => return,
            }
            conns.prune();
            if conns.has_free() {
                incoming.want();
            }
            let status =
                LinkStatus::new(Timestamp::now(), conns.stats(), dropped + inbox.dropped());
            report(&status, &mut reported, link_status, log);
        }
    }
}

/// Writes the status when its counters moved, faulting the packets no port saw.
fn report(
    status: &LinkStatus,
    reported: &mut LinkStatus,
    link_status: &mut Output<LinkStatus>,
    log: &mut Log,
) {
    if status.inbound_dropped > reported.inbound_dropped {
        let dropped = status.inbound_dropped - reported.inbound_dropped;
        log.fault("inbound_dropped", format!("{dropped} packets dropped"));
    }
    if status.changed(reported) {
        let _ = link_status.write(status);
        *reported = *status;
    }
}

/// The message ports records are routed to, and the longest record any takes.
///
/// A frame port is faulted and skipped: this link routes messages only. So is
/// a second port on one record's id, which no peer could address separately.
fn routes<'a>(
    outputs: &'a mut DynOutputs,
    log: &mut Log,
) -> (Vec<(PacketId, &'a mut Output<Bytes>)>, usize) {
    let mut ports = Vec::new();
    let mut max_len = 0;
    for (def, output) in outputs.iter_mut() {
        match &def.schema {
            RecordSchema::Msg { id, .. } if ports.iter().any(|(seen, _)| seen == id) => log.fault(
                "duplicate_id",
                format!(
                    "port `{}` carries a record an earlier port already takes",
                    def.name
                ),
            ),
            RecordSchema::Msg { id, .. } => {
                max_len = max_len.max(def.max_len);
                ports.push((*id, output));
            }
            RecordSchema::Frame { .. } => log.fault(
                "frame_output",
                format!("port `{}` carries a frame, which no link routes", def.name),
            ),
        }
    }
    (ports, max_len)
}

/// Writes every waiting packet onto its port, counting the ones no ring took.
fn deliver(inbox: &Inbox, ports: &mut [(PacketId, &mut Output<Bytes>)]) -> u64 {
    let mut full = 0;
    inbox.take(|id, bytes| {
        let Some((_, port)) = ports.iter_mut().find(|(port, _)| *port == id) else {
            return;
        };
        if port.write_bytes(bytes).is_err() {
            full += 1;
        }
    });
    full
}

/// What woke the link's loop.
enum Event {
    Connected(stellarator::net::TcpStream),
    Inbound,
    Closed,
    Stop,
}

/// The first of a connection, a packet, a connection ending, and the stop.
async fn next(incoming: &Incoming, inbox: &Inbox, conns: &Connections, stop: &Stop) -> Event {
    let connected = async { Event::Connected(incoming.next().await) };
    let inbound = async {
        inbox.stirred().await;
        Event::Inbound
    };
    let closed = async {
        conns.ended().await;
        Event::Closed
    };
    let stopped = async {
        stop.wait().await;
        Event::Stop
    };
    let first = futures_lite::future::or(connected, inbound);
    futures_lite::future::or(futures_lite::future::or(first, closed), stopped).await
}

/// One inbound packet, copied out of a connection's buffer.
struct Slot {
    id: PacketId,
    bytes: Vec<u8>,
}

/// The packets the connections' read tasks hand the loop.
///
/// A read task cannot borrow the ports, so it copies into these slots, which
/// are allocated once and hold the largest record any port takes.
struct Inbox {
    slots: RefCell<Vec<Slot>>,
    head: Cell<usize>,
    len: Cell<usize>,
    dropped: Cell<u64>,
    /// Whether a packet arrived or was dropped since the loop last looked.
    stirred: Cell<bool>,
    woken: WaitQueue,
    ids: Vec<PacketId>,
}

impl Inbox {
    fn new(ids: Vec<PacketId>, cap: usize, max_len: usize) -> Self {
        let slots = (0..cap)
            .map(|_| Slot {
                id: [0, 0],
                bytes: Vec::with_capacity(max_len),
            })
            .collect();
        Self {
            slots: RefCell::new(slots),
            head: Cell::new(0),
            len: Cell::new(0),
            dropped: Cell::new(0),
            stirred: Cell::new(false),
            woken: WaitQueue::new(),
            ids,
        }
    }

    /// Takes one packet a port asked for; a table, or an id past the queue's
    /// room, is a drop. Every other packet is the peer's business.
    fn accept(&self, packet: &Packet) {
        let OwnedPacket::Msg(msg) = packet else {
            self.drop_one();
            return;
        };
        if !self.ids.contains(&msg.id) {
            return;
        }
        let mut slots = self.slots.borrow_mut();
        let (len, cap) = (self.len.get(), slots.len());
        if len == cap {
            drop(slots);
            self.drop_one();
            return;
        }
        // PANIC Safety: the queue has room, so the slot after its tail exists.
        let slot = &mut slots[(self.head.get() + len) % cap];
        if msg.buf.len() > slot.bytes.capacity() {
            drop(slots);
            self.drop_one();
            return;
        }
        slot.id = msg.id;
        slot.bytes.clear();
        slot.bytes.extend_from_slice(&msg.buf);
        self.len.set(len + 1);
        drop(slots);
        self.stir();
    }

    /// Counts one packet no port will see, and stirs the loop so it reports it.
    fn drop_one(&self) {
        self.dropped.set(self.dropped.get() + 1);
        self.stir();
    }

    fn stir(&self) {
        self.stirred.set(true);
        self.woken.wake_all();
    }

    /// Hands every waiting packet to `f`, freeing its slot.
    fn take(&self, mut f: impl FnMut(PacketId, &[u8])) {
        let slots = self.slots.borrow();
        for at in 0..self.len.get() {
            let slot = &slots[(self.head.get() + at) % slots.len()];
            f(slot.id, &slot.bytes);
        }
        self.head
            .set((self.head.get() + self.len.get()) % slots.len().max(1));
        self.len.set(0);
        self.stirred.set(false);
    }

    /// Resolves once a packet arrived or was dropped.
    async fn stirred(&self) {
        let _ = self.woken.wait_for(|| self.stirred.get()).await;
    }

    fn dropped(&self) -> u64 {
        self.dropped.get()
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
    use metor_proto::types::PacketTy;

    use crate::port::ring_capacity;
    use crate::record::Record;
    use crate::system::{OutputBinding, PortDef, SystemOutputs};
    use crate::tests::utils::{Fixed, Imu, Note};

    use super::*;

    /// A packet as a connection's read task frames it.
    fn packet(ty: PacketTy, id: PacketId, payload: &[u8]) -> Packet {
        let mut bytes = vec![ty as u8, id[0], id[1], 0];
        bytes.extend_from_slice(payload);
        let len = bytes.len();
        let buf =
            metor_proto::buf::IoBuf::try_slice(crate::link::conn::Capped::filled(bytes), 0..len)
                .expect("the whole buffer");
        OwnedPacket::parse_with_offset(buf, 0).expect("a framed packet")
    }

    fn msg(id: PacketId, payload: &[u8]) -> Packet {
        packet(PacketTy::Msg, id, payload)
    }

    fn table(id: PacketId) -> Packet {
        packet(PacketTy::Table, id, &[0u8; 4])
    }

    fn id_of<R: Record>() -> PacketId {
        R::schema().packet_id()
    }

    fn ring<R: Record>() -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(R::MAX_LEN, 2).expect("valid capacity"),
            max_readers: 1,
        })
    }

    /// One `Fixed` port and one `Note` port, as the config would list them.
    fn ports(rings: &[RingBuffer; 2]) -> DynOutputs {
        bind(
            [
                Output::<Fixed>::def("cmds.fixed"),
                Output::<Note>::def("cmds.note"),
            ],
            rings,
        )
    }

    fn bind(defs: [PortDef; 2], rings: &[RingBuffer; 2]) -> DynOutputs {
        DynOutputs::bind(
            defs.into_iter()
                .zip(rings)
                .map(|(def, ring)| OutputBinding {
                    def,
                    writer: ring.writer(NoWake).expect("a free writer"),
                })
                .collect(),
        )
    }

    fn inbox(cap: usize) -> Inbox {
        Inbox::new(vec![id_of::<Fixed>(), id_of::<Note>()], cap, Note::MAX_LEN)
    }

    #[test]
    fn a_packet_lands_on_the_port_whose_record_carries_its_id() {
        let rings = [ring::<Fixed>(), ring::<Note>()];
        let mut outputs = ports(&rings);
        let mut log = Log;
        let (mut routed, max_len) = routes(&mut outputs, &mut log);
        assert_eq!(max_len, Note::MAX_LEN);

        let mut view = rings[1].view(NoWake).expect("a free slot");
        let inbox = inbox(4);
        inbox.accept(&msg(id_of::<Note>(), b"hello"));
        assert_eq!(deliver(&inbox, &mut routed), 0);
        assert_eq!(view.drain().next().expect("a record"), Ok(&b"hello"[..]));
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn an_id_no_port_carries_is_the_peers_business() {
        let inbox = inbox(4);
        inbox.accept(&msg([9, 9], b"probe"));
        let mut seen = 0;
        inbox.take(|_, _| seen += 1);
        assert_eq!((seen, inbox.dropped()), (0, 0));
    }

    #[test]
    fn a_table_packet_is_counted_until_a_link_routes_frames() {
        let inbox = inbox(4);
        inbox.accept(&table([1, 2]));
        assert_eq!(inbox.dropped(), 1);
    }

    #[test]
    fn a_full_queue_counts_the_packets_it_has_no_room_for() {
        let inbox = inbox(2);
        for _ in 0..3 {
            inbox.accept(&msg(id_of::<Fixed>(), b"one"));
        }
        assert_eq!(inbox.dropped(), 1);
        let mut seen = 0;
        inbox.take(|_, bytes| {
            seen += 1;
            assert_eq!(bytes, b"one");
        });
        assert_eq!(seen, 2);
        // The freed slots take the next packets, in order.
        inbox.accept(&msg(id_of::<Fixed>(), b"two"));
        inbox.take(|_, bytes| assert_eq!(bytes, b"two"));
        assert_eq!(inbox.dropped(), 1);
    }

    #[test]
    fn a_record_longer_than_its_port_never_reaches_the_ring() {
        let inbox = Inbox::new(vec![id_of::<Fixed>()], 2, Fixed::MAX_LEN);
        inbox.accept(&msg(id_of::<Fixed>(), &[0u8; Fixed::MAX_LEN + 1]));
        assert_eq!(inbox.dropped(), 1);
    }

    #[test]
    fn a_record_the_inbox_takes_but_its_port_will_not_is_refused_and_counted() {
        // Both rings hold the longest record, so only a port's own bound can
        // refuse one routed to the shorter port.
        let rings = [ring::<Note>(), ring::<Note>()];
        let mut outputs = ports(&rings);
        let (mut routed, max_len) = routes(&mut outputs, &mut Log);
        let mut view = rings[0].view(NoWake).expect("a free slot");
        // The inbox bounds by the longest routed record, which is another port's.
        assert!(Fixed::MAX_LEN < 60 && 60 <= max_len);
        let inbox = inbox(2);
        inbox.accept(&msg(id_of::<Fixed>(), &[0u8; 60]));
        assert_eq!(deliver(&inbox, &mut routed), 1);
        assert!(view.drain().next().is_none());
    }

    #[test]
    fn a_second_port_on_one_record_id_is_faulted_and_left_unrouted() {
        let rings = [ring::<Fixed>(), ring::<Fixed>()];
        let mut outputs = bind(
            [
                Output::<Fixed>::def("cmds.first"),
                Output::<Fixed>::def("cmds.second"),
            ],
            &rings,
        );
        let (routed, _) = routes(&mut outputs, &mut Log);
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].0, id_of::<Fixed>());
    }

    #[test]
    fn a_frame_output_is_faulted_and_left_unrouted() {
        let rings = [ring::<Fixed>(), ring::<Imu>()];
        let mut outputs = bind(
            [
                Output::<Fixed>::def("cmds.fixed"),
                Output::<Imu>::def("cmds.imu"),
            ],
            &rings,
        );
        let (routed, max_len) = routes(&mut outputs, &mut Log);
        assert_eq!(routed.len(), 1);
        assert_eq!((routed[0].0, max_len), (id_of::<Fixed>(), Fixed::MAX_LEN));
    }

    #[test]
    fn a_full_ring_counts_a_drop_and_keeps_the_link_up() {
        let rings = [ring::<Fixed>(), ring::<Note>()];
        let mut outputs = ports(&rings);
        let mut log = Log;
        let (mut routed, _) = routes(&mut outputs, &mut log);
        let inbox = inbox(8);
        // The consumer reads nothing, so the ring fills and writes start failing.
        let _view = rings[1].view(NoWake).expect("a free slot");
        let mut full = 0;
        for _ in 0..64 {
            for _ in 0..8 {
                inbox.accept(&msg(id_of::<Note>(), b"filler"));
            }
            full += deliver(&inbox, &mut routed);
        }
        assert!(full > 0);
    }
}
