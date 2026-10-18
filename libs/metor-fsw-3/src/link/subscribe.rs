//! `Subscribe`, the link that receives records from peers and writes them
//! onto the target's output ports.

use core::ops::{Deref, DerefMut};
use std::collections::HashMap;
use std::rc::Rc;

use futures_lite::future;
use metor_fsw_3_ring::{NoWake, Notifier, View, WakeSink};
use metor_proto::types::{IntoLenPacket, PacketId, PacketTy};
use metor_proto_wkt::MsgStream;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError};
use crate::fn_system::{Cycle, Defs, Param, ParamNames, Views, Writers};
use crate::log::Log;
use crate::port::{DynOutputs, InputWake, Output};
use crate::record::{Bytes, RecordSchema};
use crate::system::PortDef;
use crate::{Stop, system};

use super::conn::{ConnCap, Connections};
use super::inbox::{self, Inbox};
use super::transport::Endpoint;
use super::wire;
use super::{LinkParams, LinkStatus, StatusReporter};

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
    #[serde(default)]
    pub conn_cap: ConnCap,
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
    pub fn new(params: SubscribeParams) -> Result<Self, MakeError> {
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
    outputs: &mut SubscribeOutputs,
    link_status: &mut Output<LinkStatus>,
    log: &mut Log,
    stop: Stop,
) {
    let mut routes = Routes::of(outputs);
    let msgs: Vec<PacketId> = routes.ids(PacketTy::Msg);
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
    let max_len = routes.max_len;
    let Ok((inbox, mut view, mut missing)) =
        Inbox::new(&routes.keys, *inbound_cap, max_len, dialing)
    else {
        log.fault(
            "inbound_cap",
            format!("no ring holds {inbound_cap} records of {max_len} bytes"),
        );
        return;
    };
    let inbox = Rc::new(inbox);
    let (mut conns, serving) = Connections::new(endpoint.slots(), *conn_cap, seed, inbox.clone());
    let _source = stellarator::spawn(endpoint.run(serving, stop.clone())).drop_guard();
    let mut dropped = 0;
    let mut reporter = StatusReporter::default();
    let wake = view.wake().clone();
    let run = async {
        loop {
            let ready = wake.wait_until(|| view.has_record() || missing.has_record());
            future::or(ready, conns.changed()).await;
            dropped += routes.deliver(&mut view);
            report_missing(&mut missing, &routes.tables, log);
            let status = LinkStatus::new(conns.stats(), dropped + inbox.dropped());
            reporter.report(status, link_status, log);
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

/// A `Subscribe`'s outputs: one dynamic port per record it receives.
///
/// A record is routed by its packet type and id, so the definition rejects
/// two ports that carry the same one.
pub struct SubscribeOutputs(DynOutputs);

impl Deref for SubscribeOutputs {
    type Target = DynOutputs;
    fn deref(&self) -> &DynOutputs {
        &self.0
    }
}

impl DerefMut for SubscribeOutputs {
    fn deref_mut(&mut self) -> &mut DynOutputs {
        &mut self.0
    }
}

impl Param for SubscribeOutputs {
    type In<W: InputWake> = ();
    type Out = SubscribeOutputs;
    type Item<'a, W: InputWake> = &'a mut SubscribeOutputs;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        let first = defs.outputs.len();
        <DynOutputs as Param>::append_defs(names, cx, defs)?;
        check_route_keys(&defs.outputs[first..])
    }

    fn bind_in<W: InputWake>(_views: &mut Views<W>) {}

    fn bind_out(
        names: &mut ParamNames<'_>,
        writers: &mut Writers,
    ) -> Result<SubscribeOutputs, MakeError> {
        <DynOutputs as Param>::bind_out(names, writers).map(SubscribeOutputs)
    }

    fn get<'a, W: InputWake>(
        _input: &'a mut (),
        output: &'a mut SubscribeOutputs,
        _cx: &mut Cycle<'a>,
    ) -> &'a mut SubscribeOutputs {
        output
    }
}

/// Rejects two ports that carry the same packet type and id, since no peer
/// could address them separately.
fn check_route_keys(ports: &[PortDef]) -> Result<(), DefError> {
    let mut seen: HashMap<(PacketTy, PacketId), &str> = HashMap::with_capacity(ports.len());
    for port in ports {
        let key = (port.schema.packet_ty(), port.schema.packet_id());
        if let Some(other) = seen.insert(key, &port.name) {
            return Err(DefError::RecordIdCollision {
                port: port.name.to_string(),
                other: other.to_string(),
            });
        }
    }
    Ok(())
}

/// The routing table for a `Subscribe`'s output ports.
struct Routes<'a> {
    /// Each output port.
    ports: Vec<&'a mut Output<Bytes>>,
    /// The packet type and id each port's records arrive under, in port
    /// order.
    keys: Vec<(PacketTy, PacketId)>,
    /// The index of the port for each packet type and id.
    index: HashMap<(PacketTy, PacketId), usize>,
    /// The longest record any port accepts.
    max_len: usize,
    /// The table id and name of every frame port.
    tables: Vec<(PacketId, String)>,
}

impl<'a> Routes<'a> {
    /// Builds the routing table for `outputs`.
    ///
    /// A message port is keyed by its message id, and a frame port by its
    /// vtable's table id. The definition already rejected two ports with the
    /// same key.
    fn of(outputs: &'a mut DynOutputs) -> Self {
        let mut routes = Routes {
            ports: Vec::new(),
            keys: Vec::new(),
            index: HashMap::new(),
            max_len: 0,
            tables: Vec::new(),
        };
        for (index, (def, output)) in outputs.iter_mut().enumerate() {
            let key = (def.schema.packet_ty(), def.schema.packet_id());
            if let RecordSchema::Frame { .. } = def.schema {
                routes.tables.push((key.1, def.name.to_string()));
            }
            routes.max_len = routes.max_len.max(def.max_len);
            routes.keys.push(key);
            routes.index.insert(key, index);
            routes.ports.push(output);
        }
        routes
    }

    /// Returns the ids of the ports with packet type `ty`.
    fn ids(&self, ty: PacketTy) -> Vec<PacketId> {
        self.keys
            .iter()
            .filter(|(t, _)| *t == ty)
            .map(|(_, id)| *id)
            .collect()
    }

    /// Writes every record waiting in the inbox onto its port. Returns how
    /// many records were dropped because the port's ring was full.
    fn deliver(&mut self, view: &mut View<Notifier>) -> u64 {
        let mut full = 0;
        for record in view.drain() {
            let Some((ty, id, bytes)) = record.ok().and_then(inbox::packet) else {
                continue;
            };
            let Some(&index) = self.index.get(&(ty, id)) else {
                continue;
            };
            if self.ports[index].write(bytes).is_err() {
                full += 1;
            }
        }
        view.settle();
        full
    }
}

#[cfg(test)]
mod tests {
    use super::super::inbox::ConnectionState;
    use metor_fsw_3_ring::{NoWake, RingBuffer};
    use zerocopy::IntoBytes;

    use crate::DefCx;

    use crate::record::{Payload, Record};
    use crate::system::{OutputBinding, PortDef};
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
        DynOutputs::new(
            defs.into_iter()
                .zip(rings)
                .map(|(def, ring)| OutputBinding {
                    def,
                    writer: ring.writer(NoWake).expect("a free writer"),
                })
                .collect(),
            &std::sync::Arc::default(),
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
        let mut routes = Routes::of(&mut outputs);
        assert_eq!(routes.max_len, Note::MAX_LEN);
        assert!(routes.tables.is_empty());

        let mut note = rings[1].view(NoWake).expect("a free slot");
        let (inbox, mut view) = inbox(4);
        inbox.route(
            &mut ConnectionState::Streaming,
            &msg(id_of::<Note>(), b"hello"),
        );
        assert_eq!(routes.deliver(&mut view), 0);
        assert_eq!(note.drain().next().expect("a record"), Ok(&b"hello"[..]));
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_route_table_by_table_id() {
        let rings = [ring::<Fixed>(2, 1), ring::<Imu>(2, 1)];
        let mut outputs = mixed(&rings);
        let mut routes = Routes::of(&mut outputs);
        assert_eq!(routes.max_len, Imu::MAX_LEN);
        let table = Output::<Imu>::def(&DefCx::empty(), "plant.imu")
            .schema
            .packet_id();
        assert_ne!(
            table,
            id_of::<Imu>(),
            "a table id is the port's, not the record's"
        );
        assert_eq!(routes.tables, vec![(table, "plant.imu".to_string())]);
        assert_eq!(routes.ids(PacketTy::Msg), vec![id_of::<Fixed>()]);
        assert_eq!(routes.ids(PacketTy::Table), vec![table]);

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
        assert_eq!(routes.deliver(&mut view), 0);
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
        let mut routes = Routes::of(&mut outputs);
        let mut fixed = rings[0].view(NoWake).expect("a free slot");
        // The inbox accepts records up to the longest port's limit, which here
        // belongs to the other port.
        assert!(Fixed::MAX_LEN < 60 && 60 <= routes.max_len);
        let (inbox, mut view) = inbox(2);
        inbox.route(
            &mut ConnectionState::Streaming,
            &msg(id_of::<Fixed>(), &[0u8; 60]),
        );
        assert_eq!(routes.deliver(&mut view), 1);
        assert!(fixed.drain().next().is_none());
    }

    #[test]
    fn test_reject_duplicate_record_route() {
        let ports = [
            Output::<Fixed>::def(&DefCx::empty(), "cmds.first"),
            Output::<Fixed>::def(&DefCx::empty(), "cmds.second"),
        ];
        assert_eq!(
            check_route_keys(&ports),
            Err(DefError::RecordIdCollision {
                port: "cmds.second".into(),
                other: "cmds.first".into(),
            })
        );
        assert_eq!(check_route_keys(&ports[..1]), Ok(()));
    }

    /// Two ports of the same frame type under different names have different
    /// tables. Two ports that share one table are refused.
    #[test]
    fn test_reject_duplicate_table_route() {
        let distinct = [
            Output::<Imu>::def(&DefCx::empty(), "a.imu"),
            Output::<Imu>::def(&DefCx::empty(), "b.imu"),
        ];
        assert_eq!(check_route_keys(&distinct), Ok(()));

        let def = Output::<Imu>::def(&DefCx::empty(), "a.imu");
        let copy = PortDef {
            name: "copy".into(),
            ..def.clone()
        };
        assert!(matches!(
            check_route_keys(&[def, copy]),
            Err(DefError::RecordIdCollision { .. })
        ));
    }

    #[test]
    fn test_ring_overflow_counts_drops() {
        let rings = [ring::<Fixed>(2, 1), ring::<Note>(2, 1)];
        let mut outputs = ports(&rings);
        let mut routes = Routes::of(&mut outputs);
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
            full += routes.deliver(&mut view);
        }
        assert!(full > 0);
    }
}
