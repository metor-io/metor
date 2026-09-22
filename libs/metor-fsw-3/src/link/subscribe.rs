//! `Subscribe`: the records a target is sent, written onto its rings.

use std::rc::Rc;

use futures_lite::future;
use metor_fsw_3_ring::{Notifier, View, WakeSink};
use metor_proto::types::{PacketId, Timestamp};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::coordinator::ParamError;
use crate::log::Log;
use crate::port::{DynOutputs, Output};
use crate::record::{Bytes, RecordSchema};
use crate::{Stop, system};

use super::conn::{self, Connections, Event};
use super::inbox::{self, Inbox};
use super::transport::{Endpoint, Transport, source};
use super::wire;
use super::{LinkStatus, default_conn_cap};

/// Records a link holds for its ports, at least, before it drops one.
const DEFAULT_INBOUND_CAP: usize = 256;

fn default_inbound_cap() -> usize {
    DEFAULT_INBOUND_CAP
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
    /// The size of the per-connection pending write buffer.
    #[serde(default = "default_conn_cap")]
    pub conn_cap: usize,
    #[serde(default = "default_inbound_cap")]
    pub inbound_cap: usize,
}

/// Writes every message its peers send onto the port that carries it.
pub struct Subscribe {
    endpoint: Option<Endpoint>,
    namespace: Option<String>,
    link: String,
    conn_cap: usize,
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
            conn_cap: params.conn_cap,
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

        let seed = match endpoint {
            Endpoint::Listen { .. } => {
                wire::link_info(ids.clone(), self.namespace.as_deref(), &self.link)
            }
            Endpoint::Connect { .. } => Vec::new(),
        };

        let Some((inbox, mut view)) = Inbox::new(ids, self.inbound_cap, max_len) else {
            let cap = self.inbound_cap;
            log.fault(
                "inbound_cap",
                format!("no ring holds {cap} records of {max_len} bytes"),
            );
            return;
        };
        let inbox = Rc::new(inbox);
        let conns = Rc::new(Connections::new(
            endpoint.slots(),
            self.conn_cap,
            seed,
            inbox.clone(),
        ));
        let _source =
            stellarator::spawn(source(endpoint, conns.clone(), stop.clone())).drop_guard();
        let mut dropped = 0;
        let mut reported = LinkStatus::new(Timestamp(0), Default::default(), 0);
        loop {
            let ready = async {
                view.wake().wait_until(|| view.has_record()).await;
                Event::Ready
            };
            let event = future::or(conn::next(&conns, &stop), ready).await;
            match event {
                Event::Ready => dropped += deliver(&mut view, &mut ports),
                Event::Changed => {}
                Event::Stop => return,
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

/// Writes every waiting record onto its port, counting the ones no ring took.
fn deliver(view: &mut View<Notifier>, ports: &mut [(PacketId, &mut Output<Bytes>)]) -> u64 {
    let mut full = 0;
    for record in view.drain() {
        let Some((id, bytes)) = record.ok().and_then(inbox::message) else {
            continue;
        };
        let Some((_, port)) = ports.iter_mut().find(|(port, _)| *port == id) else {
            continue;
        };
        if port.write_bytes(bytes).is_err() {
            full += 1;
        }
    }
    view.settle();
    full
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

    /// A message packet's body as the wire carries it.
    fn msg(id: PacketId, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![PacketTy::Msg as u8, id[0], id[1], 0];
        body.extend_from_slice(payload);
        body
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

    fn inbox(cap: usize) -> (Inbox, View<Notifier>) {
        Inbox::new(vec![id_of::<Fixed>(), id_of::<Note>()], cap, Note::MAX_LEN)
            .expect("a small ring")
    }

    #[test]
    fn test_route_packet_by_record_id() {
        let rings = [ring::<Fixed>(), ring::<Note>()];
        let mut outputs = ports(&rings);
        let mut log = Log;
        let (mut routed, max_len) = routes(&mut outputs, &mut log);
        assert_eq!(max_len, Note::MAX_LEN);

        let mut note = rings[1].view(NoWake).expect("a free slot");
        let (inbox, mut view) = inbox(4);
        inbox.accept(&msg(id_of::<Note>(), b"hello"));
        assert_eq!(deliver(&mut view, &mut routed), 0);
        assert_eq!(note.drain().next().expect("a record"), Ok(&b"hello"[..]));
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_port_rejects_oversized_record() {
        // Both rings hold the longest record, so only a port's own bound can
        // refuse one routed to the shorter port.
        let rings = [ring::<Note>(), ring::<Note>()];
        let mut outputs = ports(&rings);
        let (mut routed, max_len) = routes(&mut outputs, &mut Log);
        let mut fixed = rings[0].view(NoWake).expect("a free slot");
        // The inbox bounds by the longest routed record, which is another port's.
        assert!(Fixed::MAX_LEN < 60 && 60 <= max_len);
        let (inbox, mut view) = inbox(2);
        inbox.accept(&msg(id_of::<Fixed>(), &[0u8; 60]));
        assert_eq!(deliver(&mut view, &mut routed), 1);
        assert!(fixed.drain().next().is_none());
    }

    #[test]
    fn test_reject_duplicate_record_route() {
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
    fn test_reject_frame_route() {
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
    fn test_ring_overflow_counts_drops() {
        let rings = [ring::<Fixed>(), ring::<Note>()];
        let mut outputs = ports(&rings);
        let mut log = Log;
        let (mut routed, _) = routes(&mut outputs, &mut log);
        let (inbox, mut view) = inbox(8);
        // The consumer reads nothing, so the ring fills and writes start failing.
        let _note = rings[1].view(NoWake).expect("a free slot");
        let mut full = 0;
        for _ in 0..64 {
            for _ in 0..8 {
                inbox.accept(&msg(id_of::<Note>(), b"filler"));
            }
            full += deliver(&mut view, &mut routed);
        }
        assert!(full > 0);
    }
}
