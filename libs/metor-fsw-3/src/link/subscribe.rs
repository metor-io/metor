//! `Subscribe`, the link that receives records from peers and writes them
//! onto the target's output ports.

use std::rc::Rc;

use futures_lite::future;
use metor_fsw_3_ring::{NoWake, Notifier, View, WakeSink};
use metor_proto::types::{IntoLenPacket, PacketId, PacketTy};
use metor_proto_wkt::MsgStream;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::coordinator::ParamError;
use crate::log::Log;
use crate::port::{DynOutputs, Output};
use crate::record::{Bytes, RecordSchema};
use crate::{Stop, system};

use super::conn::Connections;
use super::inbox::{self, Inbox};
use super::transport::Endpoint;
use super::wire;
use super::{LinkParams, LinkStatus, default_conn_cap, report};

/// The default minimum number of records the inbox holds before it starts
/// dropping them.
fn default_inbound_cap() -> usize {
    256
}

/// The configuration for a [`Subscribe`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubscribeParams {
    #[serde(flatten)]
    pub link: LinkParams,
    /// The size in bytes of each connection's pending write buffer. It also
    /// sizes each connection's receive buffer.
    #[serde(default = "default_conn_cap")]
    pub conn_cap: usize,
    #[serde(default = "default_inbound_cap")]
    pub inbound_cap: usize,
}

/// A link that receives records from its peers and writes each one onto the
/// output port for that record's id.
pub struct Subscribe {
    endpoint: Option<Endpoint>,
    params: SubscribeParams,
}

impl Subscribe {
    /// Binds the transport. This happens at build time, so a taken address
    /// fails the build.
    pub fn new(params: SubscribeParams) -> Result<Self, ParamError> {
        let endpoint = params.link.transport.bind()?;
        Ok(Self {
            endpoint: Some(endpoint),
            params,
        })
    }
}

/// Runs the `Subscribe` link.
///
/// A listening link sends each peer a `LinkInfo` listing the messages it
/// accepts. A dialing link instead asks its peer to stream those messages,
/// and checks that the peer announces every table an output port expects.
/// Either way, every packet that arrives is routed to an output port by its
/// type and id.
#[system]
pub async fn subscribe(
    #[state] s: &mut Subscribe,
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
    let routes: Vec<(PacketTy, PacketId)> = ports.iter().map(|(ty, id, _)| (*ty, *id)).collect();
    let SubscribeParams {
        link: LinkParams {
            namespace, link, ..
        },
        conn_cap,
        inbound_cap,
    } = &s.params;

    // PANIC Safety: `new` always sets the endpoint, and a system runs once.
    let endpoint = s.endpoint.take().expect("one run per instance");
    if let Some(addr) = endpoint.local_addr() {
        log.info(format!("listening on {addr}"));
    }

    let seed = match endpoint {
        Endpoint::Listen { .. } => wire::link_info(msgs.clone(), namespace.as_deref(), link),
        // A db peer answers these by forwarding the message logs. A
        // `Publish` peer ignores them.
        Endpoint::Connect { .. } => msgs
            .iter()
            .flat_map(|&msg_id| (&MsgStream { msg_id }).into_len_packet().inner)
            .collect(),
    };

    let dialing = matches!(endpoint, Endpoint::Connect { .. });
    let Some((inbox, mut view, mut missing)) = Inbox::new(&routes, *inbound_cap, max_len, dialing)
    else {
        log.fault(
            "inbound_cap",
            format!("no ring holds {inbound_cap} records of {max_len} bytes"),
        );
        return;
    };
    let inbox = Rc::new(inbox);
    let Some(conns) = Connections::new(endpoint.slots(), *conn_cap, seed, inbox.clone()) else {
        log.fault("conn_cap", format!("no ring holds a {conn_cap}-byte batch"));
        return;
    };
    let conns = Rc::new(conns);
    let _source = stellarator::spawn(endpoint.run(conns.clone(), stop.clone())).drop_guard();
    let mut dropped = 0;
    let mut reported = LinkStatus::default();
    let run = async {
        loop {
            let ready = view
                .wake()
                .wait_until(|| view.has_record() || missing.has_record());
            future::or(ready, conns.changed()).await;
            dropped += deliver(&mut view, &mut ports);
            report_missing(&mut missing, &tables, log);
            let status = LinkStatus::new(conns.stats(), dropped + inbox.dropped());
            report(&status, &mut reported, link_status, log);
        }
    };
    future::or(run, stop.wait()).await;
}

/// Logs a fault for each frame port whose table the peer did not announce.
///
/// This usually means the peer was rebuilt with a changed frame type, so its
/// table id no longer matches.
fn report_missing(missing: &mut View<NoWake>, tables: &[(PacketId, String)], log: &mut Log) {
    for id in missing.drain().filter_map(Result::ok) {
        let Some((_, port)) = tables.iter().find(|(table, _)| table[..] == *id) else {
            continue;
        };
        log.fault(
            "peer_schema_mismatch",
            format!("port `{port}` was built against a table the peer does not announce"),
        );
    }
    missing.settle();
}

/// The routing table for a `Subscribe`'s output ports.
struct Routes<'a> {
    /// Each output port, keyed by the packet type and id its records arrive
    /// under.
    ports: Vec<(PacketTy, PacketId, &'a mut Output<Bytes>)>,
    /// The longest record any port accepts.
    max_len: usize,
    /// The table id and name of every frame port.
    tables: Vec<(PacketId, String)>,
}

/// Builds the routing table for `outputs`.
///
/// A message port is keyed by its message id, and a frame port by its
/// vtable's table id. If a port has the same key as an earlier port, it is
/// skipped and a fault is logged, since no peer could address it separately.
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

/// Returns the ids of the ports with packet type `ty`.
fn ids(ports: &[(PacketTy, PacketId, &mut Output<Bytes>)], ty: PacketTy) -> Vec<PacketId> {
    ports
        .iter()
        .filter(|(t, ..)| *t == ty)
        .map(|(_, id, _)| *id)
        .collect()
}

/// Writes every record waiting in the inbox onto its port. Returns how many
/// records were dropped because the port's ring was full.
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
    use super::super::inbox::ConnectionState;
    use metor_fsw_3_ring::{NoWake, RingBuffer};
    use zerocopy::IntoBytes;

    use crate::DefCx;

    use crate::record::Record;
    use crate::system::{OutputBinding, PortDef, Ports};
    use crate::tests::utils::{Fixed, Imu, Note, ring};

    use super::*;

    /// Builds a packet body in its wire format.
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

    /// Builds a `Fixed` port and a `Note` port, as a config would list them.
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

    fn route_set(msgs: &[PacketId], tables: &[PacketId]) -> Vec<(PacketTy, PacketId)> {
        let msgs = msgs.iter().map(|&id| (PacketTy::Msg, id));
        msgs.chain(tables.iter().map(|&id| (PacketTy::Table, id)))
            .collect()
    }

    fn inbox(cap: usize) -> (Inbox, View<Notifier>) {
        let routes = route_set(&[id_of::<Fixed>(), id_of::<Note>()], &[]);
        let (inbox, view, _) =
            Inbox::new(&routes, cap, Note::MAX_LEN, false).expect("a small ring");
        (inbox, view)
    }

    /// Builds a `Fixed` message port and an `Imu` frame port.
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
        let rings = [ring::<Fixed>(2, 1), ring::<Note>(2, 1)];
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
        inbox.route(
            &mut ConnectionState::Streaming,
            &msg(id_of::<Note>(), b"hello"),
        );
        assert_eq!(deliver(&mut view, &mut ports), 0);
        assert_eq!(note.drain().next().expect("a record"), Ok(&b"hello"[..]));
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_route_table_by_table_id() {
        let rings = [ring::<Fixed>(2, 1), ring::<Imu>(2, 1)];
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
        let (inbox, mut view, _) = Inbox::new(
            &route_set(&[id_of::<Fixed>()], &[table]),
            4,
            Imu::MAX_LEN,
            false,
        )
        .expect("a ring");
        let frame = Imu::new(3, 4.0);
        inbox.route(
            &mut ConnectionState::Streaming,
            &body(PacketTy::Table, table, frame.as_bytes()),
        );
        inbox.route(
            &mut ConnectionState::Streaming,
            &body(PacketTy::Table, [9, 9], frame.as_bytes()),
        );
        assert_eq!(deliver(&mut view, &mut ports), 0);
        let received = imu.drain().next().expect("a record").expect("intact");
        assert_eq!(Imu::decode(received), Ok(&frame));
        assert!(imu.drain().next().is_none(), "the unknown table is ignored");
    }

    #[test]
    fn test_port_rejects_oversized_record() {
        // Both rings fit the longest record, so only the port's own length
        // limit can reject a long record sent to the shorter port.
        let rings = [ring::<Note>(2, 1), ring::<Note>(2, 1)];
        let mut outputs = ports(&rings);
        let Routes {
            mut ports, max_len, ..
        } = routes(&mut outputs, &mut Log);
        let mut fixed = rings[0].view(NoWake).expect("a free slot");
        // The inbox accepts records up to the longest port's limit, which here
        // belongs to the other port.
        assert!(Fixed::MAX_LEN < 60 && 60 <= max_len);
        let (inbox, mut view) = inbox(2);
        inbox.route(
            &mut ConnectionState::Streaming,
            &msg(id_of::<Fixed>(), &[0u8; 60]),
        );
        assert_eq!(deliver(&mut view, &mut ports), 1);
        assert!(fixed.drain().next().is_none());
    }

    #[test]
    fn test_reject_duplicate_record_route() {
        let rings = [ring::<Fixed>(2, 1), ring::<Fixed>(2, 1)];
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

    /// Two ports of the same frame type under different names have different
    /// tables. Two ports that share one table get a single route.
    #[test]
    fn test_reject_duplicate_table_route() {
        let rings = [ring::<Imu>(2, 1), ring::<Imu>(2, 1)];
        let mut distinct = bind(
            [
                Output::<Imu>::def(&DefCx::empty(), "a.imu"),
                Output::<Imu>::def(&DefCx::empty(), "b.imu"),
            ],
            &rings,
        );
        assert_eq!(routes(&mut distinct, &mut Log).tables.len(), 2);

        let def = Output::<Imu>::def(&DefCx::empty(), "a.imu");
        let rings = [ring::<Imu>(2, 1), ring::<Imu>(2, 1)];
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
        let rings = [ring::<Fixed>(2, 1), ring::<Note>(2, 1)];
        let mut outputs = ports(&rings);
        let mut log = Log;
        let Routes { mut ports, .. } = routes(&mut outputs, &mut log);
        let (inbox, mut view) = inbox(8);
        // Nothing reads the ring, so it fills up and writes start failing.
        let _note = rings[1].view(NoWake).expect("a free slot");
        let mut full = 0;
        for _ in 0..64 {
            for _ in 0..8 {
                inbox.route(
                    &mut ConnectionState::Streaming,
                    &msg(id_of::<Note>(), b"filler"),
                );
            }
            full += deliver(&mut view, &mut ports);
        }
        assert!(full > 0);
    }
}
