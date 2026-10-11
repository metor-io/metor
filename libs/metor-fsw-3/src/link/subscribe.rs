//! `Subscribe`: the records a target is sent, written onto its rings.

use std::rc::Rc;

use futures_lite::future;
use metor_fsw_3_ring::{Notifier, View, WakeSink};
use metor_proto::types::{PacketId, PacketTy, Timestamp};
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

/// Writes every record its peers send onto the port that carries it.
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
    /// Tells each peer what it may send, or asks a dialed peer for its
    /// message streams, then routes what arrives by its id.
    async fn run(
        &mut self,
        outputs: &mut DynOutputs,
        link_status: &mut Output<LinkStatus>,
        log: &mut Log,
        stop: Stop,
    ) {
        let Routes {
            mut ports,
            max_len,
            tables,
        } = routes(outputs, log);
        let msgs: Vec<PacketId> = ids(&ports, PacketTy::Msg);
        let table_ids: Vec<PacketId> = tables.iter().map(|(id, _)| *id).collect();

        // PANIC Safety: `run` is called once, and the endpoint is the bind's.
        let endpoint = self.endpoint.take().expect("one run per instance");
        if let Some(addr) = endpoint.local_addr() {
            log.info(format!("listening on {addr}"));
        }

        let seed = match endpoint {
            Endpoint::Listen { .. } => {
                wire::link_info(msgs.clone(), self.namespace.as_deref(), &self.link)
            }
            Endpoint::Connect { .. } => wire::msg_streams(&msgs),
        };

        let Some((inbox, mut view)) = Inbox::new(msgs, table_ids, self.inbound_cap, max_len) else {
            let cap = self.inbound_cap;
            log.fault(
                "inbound_cap",
                format!("no ring holds {cap} records of {max_len} bytes"),
            );
            return;
        };
        if matches!(endpoint, Endpoint::Connect { .. }) {
            inbox.watch();
        }
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
                view.wake()
                    .wait_until(|| view.has_record() || inbox.check_due())
                    .await;
                Event::Ready
            };
            let event = future::or(conn::next(&conns, &stop), ready).await;
            match event {
                Event::Ready => {
                    dropped += deliver(&mut view, &mut ports);
                    report_unannounced(&inbox, &tables, log);
                }
                Event::Changed => {}
                Event::Stop => return,
            }
            let status =
                LinkStatus::new(Timestamp::now(), conns.stats(), dropped + inbox.dropped());
            report(&status, &mut reported, link_status, log);
        }
    }
}

/// Faults each frame port whose table the peer never announced, which is what
/// a peer rebuilt with a changed frame looks like.
fn report_unannounced(inbox: &Inbox, tables: &[(PacketId, String)], log: &mut Log) {
    for id in inbox.unannounced_iter() {
        let Some((_, port)) = tables.iter().find(|(table, _)| *table == id) else {
            continue;
        };
        log.fault(
            "peer_schema_mismatch",
            format!("port `{port}` was built against a table the peer does not announce"),
        );
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

/// The ports records are routed to, each by the packet type and id its
/// records arrive under, the longest record any takes, and the frame ports
/// by table id.
struct Routes<'a> {
    ports: Vec<(PacketTy, PacketId, &'a mut Output<Bytes>)>,
    max_len: usize,
    tables: Vec<(PacketId, String)>,
}

/// A message port takes its record's id, a frame port its vtable's table id.
/// A second port under an id an earlier port took is faulted and skipped,
/// since no peer could address it separately.
fn routes<'a>(outputs: &'a mut DynOutputs, log: &mut Log) -> Routes<'a> {
    let mut routes = Routes {
        ports: Vec::new(),
        max_len: 0,
        tables: Vec::new(),
    };
    for (def, output) in outputs.iter_mut() {
        let (ty, id) = (def.schema.packet_ty(), def.schema.packet_id());
        if routes
            .ports
            .iter()
            .any(|(t, seen, _)| (*t, *seen) == (ty, id))
        {
            log.fault(
                "duplicate_id",
                format!(
                    "port `{}` carries a record an earlier port already takes",
                    def.name
                ),
            );
            continue;
        }
        if let RecordSchema::Frame { .. } = def.schema {
            routes.tables.push((id, def.name.to_string()));
        }
        routes.max_len = routes.max_len.max(def.max_len);
        routes.ports.push((ty, id, output));
    }
    routes
}

/// The ids of the ports routed under packet type `ty`.
fn ids(ports: &[(PacketTy, PacketId, &mut Output<Bytes>)], ty: PacketTy) -> Vec<PacketId> {
    ports
        .iter()
        .filter(|(t, ..)| *t == ty)
        .map(|(_, id, _)| *id)
        .collect()
}

/// Writes every waiting record onto its port, counting the ones no ring took.
fn deliver(
    view: &mut View<Notifier>,
    ports: &mut [(PacketTy, PacketId, &mut Output<Bytes>)],
) -> u64 {
    let mut full = 0;
    for record in view.drain() {
        let Some((ty, id, bytes)) = record.ok().and_then(inbox::packet) else {
            continue;
        };
        let ty = match ty {
            PacketTy::MsgWithTimestamp => PacketTy::Msg,
            ty => ty,
        };
        let Some((.., port)) = ports.iter_mut().find(|(t, i, _)| (*t, *i) == (ty, id)) else {
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
    use zerocopy::IntoBytes;

    use crate::DefCx;
    use crate::port::ring_capacity;
    use crate::record::Record;
    use crate::system::{OutputBinding, PortDef, SystemOutputs};
    use crate::tests::utils::{Fixed, Imu, Note};

    use super::*;

    /// A packet's body as the wire carries it.
    fn body(ty: PacketTy, id: PacketId, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![ty as u8, id[0], id[1], 0];
        body.extend_from_slice(payload);
        body
    }

    fn msg(id: PacketId, payload: &[u8]) -> Vec<u8> {
        body(PacketTy::Msg, id, payload)
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
                Output::<Fixed>::def(&DefCx::empty(), "cmds.fixed"),
                Output::<Note>::def(&DefCx::empty(), "cmds.note"),
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
        Inbox::new(
            vec![id_of::<Fixed>(), id_of::<Note>()],
            Vec::new(),
            cap,
            Note::MAX_LEN,
        )
        .expect("a small ring")
    }

    /// One `Fixed` message port and one `Imu` frame port.
    fn mixed(rings: &[RingBuffer; 2]) -> DynOutputs {
        bind(
            [
                Output::<Fixed>::def(&DefCx::empty(), "cmds.fixed"),
                Output::<Imu>::def(&DefCx::empty(), "plant.imu"),
            ],
            rings,
        )
    }

    #[test]
    fn test_route_packet_by_record_id() {
        let rings = [ring::<Fixed>(), ring::<Note>()];
        let mut outputs = ports(&rings);
        let mut log = Log;
        let Routes {
            mut ports,
            max_len,
            tables,
        } = routes(&mut outputs, &mut log);
        assert_eq!(max_len, Note::MAX_LEN);
        assert!(tables.is_empty());

        let mut note = rings[1].view(NoWake).expect("a free slot");
        let (inbox, mut view) = inbox(4);
        inbox.accept(&msg(id_of::<Note>(), b"hello"));
        assert_eq!(deliver(&mut view, &mut ports), 0);
        assert_eq!(note.drain().next().expect("a record"), Ok(&b"hello"[..]));
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_route_table_by_table_id() {
        let rings = [ring::<Fixed>(), ring::<Imu>()];
        let mut outputs = mixed(&rings);
        let mut log = Log;
        let Routes {
            mut ports,
            max_len,
            tables,
        } = routes(&mut outputs, &mut log);
        assert_eq!(max_len, Imu::MAX_LEN);
        let table = Output::<Imu>::def(&DefCx::empty(), "plant.imu")
            .schema
            .packet_id();
        assert_ne!(
            table,
            id_of::<Imu>(),
            "a table id is the port's, not the record's"
        );
        assert_eq!(tables, vec![(table, "plant.imu".to_string())]);
        assert_eq!(ids(&ports, PacketTy::Msg), vec![id_of::<Fixed>()]);
        assert_eq!(ids(&ports, PacketTy::Table), vec![table]);

        let mut imu = rings[1].view(NoWake).expect("a free slot");
        let (inbox, mut view) =
            Inbox::new(vec![id_of::<Fixed>()], vec![table], 4, Imu::MAX_LEN).expect("a ring");
        let frame = Imu::new(3, 4.0);
        inbox.accept(&body(PacketTy::Table, table, frame.as_bytes()));
        inbox.accept(&body(PacketTy::Table, [9, 9], frame.as_bytes()));
        assert_eq!(deliver(&mut view, &mut ports), 0);
        let received = imu.drain().next().expect("a record").expect("intact");
        assert_eq!(Imu::decode(received), Ok(&frame));
        assert!(imu.drain().next().is_none(), "the unknown table is ignored");
    }

    #[test]
    fn test_port_rejects_oversized_record() {
        // Both rings hold the longest record, so only a port's own bound can
        // refuse one routed to the shorter port.
        let rings = [ring::<Note>(), ring::<Note>()];
        let mut outputs = ports(&rings);
        let Routes {
            mut ports, max_len, ..
        } = routes(&mut outputs, &mut Log);
        let mut fixed = rings[0].view(NoWake).expect("a free slot");
        // The inbox bounds by the longest routed record, which is another port's.
        assert!(Fixed::MAX_LEN < 60 && 60 <= max_len);
        let (inbox, mut view) = inbox(2);
        inbox.accept(&msg(id_of::<Fixed>(), &[0u8; 60]));
        assert_eq!(deliver(&mut view, &mut ports), 1);
        assert!(fixed.drain().next().is_none());
    }

    #[test]
    fn test_reject_duplicate_record_route() {
        let rings = [ring::<Fixed>(), ring::<Fixed>()];
        let mut outputs = bind(
            [
                Output::<Fixed>::def(&DefCx::empty(), "cmds.first"),
                Output::<Fixed>::def(&DefCx::empty(), "cmds.second"),
            ],
            &rings,
        );
        let Routes { ports, .. } = routes(&mut outputs, &mut Log);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].1, id_of::<Fixed>());
    }

    /// Two ports of one frame under different names are two tables; two
    /// carrying one announced table are one route.
    #[test]
    fn test_reject_duplicate_table_route() {
        let rings = [ring::<Imu>(), ring::<Imu>()];
        let mut distinct = bind(
            [
                Output::<Imu>::def(&DefCx::empty(), "a.imu"),
                Output::<Imu>::def(&DefCx::empty(), "b.imu"),
            ],
            &rings,
        );
        assert_eq!(routes(&mut distinct, &mut Log).tables.len(), 2);

        let def = Output::<Imu>::def(&DefCx::empty(), "a.imu");
        let rings = [ring::<Imu>(), ring::<Imu>()];
        let mut same = bind(
            [
                def.clone(),
                PortDef {
                    name: "copy".into(),
                    ..def.clone()
                },
            ],
            &rings,
        );
        let Routes { ports, tables, .. } = routes(&mut same, &mut Log);
        assert_eq!(ports.len(), 1);
        assert_eq!(tables, vec![(def.schema.packet_id(), "a.imu".to_string())]);
    }

    #[test]
    fn test_ring_overflow_counts_drops() {
        let rings = [ring::<Fixed>(), ring::<Note>()];
        let mut outputs = ports(&rings);
        let mut log = Log;
        let Routes { mut ports, .. } = routes(&mut outputs, &mut log);
        let (inbox, mut view) = inbox(8);
        // The consumer reads nothing, so the ring fills and writes start failing.
        let _note = rings[1].view(NoWake).expect("a free slot");
        let mut full = 0;
        for _ in 0..64 {
            for _ in 0..8 {
                inbox.accept(&msg(id_of::<Note>(), b"filler"));
            }
            full += deliver(&mut view, &mut ports);
        }
        assert!(full > 0);
    }
}
