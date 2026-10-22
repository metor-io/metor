//! `Subscribe`, the link that receives records from peers and writes them
//! onto the target's output ports.

use core::ops::{Deref, DerefMut};
use std::collections::HashMap;

use metor_proto::types::{IntoLenPacket, PacketId, PacketTy};
use metor_proto_wkt::MsgStream;

use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError};
use crate::fn_system::{Cycle, Defs, Param, ParamNames, Views, Writers};
use crate::io::{Cap, Endpoint, IoInit};
use crate::log::Log;
use crate::port::{DynOutputs, InputWake, Output};
use crate::record::{Bytes, RecordSchema};
use crate::system::PortDef;
use crate::{Seq, SeqError, sequence};

use super::conns::{Conns, ConnsMem};
use super::{LinkParams, LinkStatus, StatusReporter, failed, made_once, wire};

/// A link that receives records from its peers and writes each one onto the
/// output port for that record's id.
pub struct Subscribe {
    params: LinkParams,
    /// The connections' buffers, until the body binds them.
    mem: Option<ConnsMem>,
}

impl Subscribe {
    /// Allocates the connections' buffers.
    pub fn new(params: LinkParams, io: &mut IoInit<'_>) -> Result<Self, MakeError> {
        let mem = ConnsMem::make(io, params.conns())?;
        Ok(Self {
            params,
            mem: Some(mem),
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
#[sequence]
pub async fn subscribe(
    #[state] s: &mut Subscribe,
    seq: &Seq,
    ep: &Cap<Endpoint>,
    outputs: &mut SubscribeOutputs,
    link_status: &mut Output<LinkStatus>,
    log: &mut Log,
) -> Result<(), SeqError> {
    let mut routes = Routes::of(outputs);
    let msgs = routes.ids(PacketTy::Msg);
    let LinkParams {
        namespace,
        link,
        dialing,
        ..
    } = &s.params;
    let seed: Vec<u8> = match dialing {
        // A db peer answers these by forwarding the message logs. A
        // `Publish` peer ignores them.
        true => msgs
            .iter()
            .flat_map(|&msg_id| (&MsgStream { msg_id }).into_len_packet().inner)
            .collect(),
        false => wire::link_info(msgs, namespace.as_deref(), link),
    };
    let tables = routes.ids(PacketTy::Table);
    let mem = s.mem.take().ok_or_else(made_once)?;
    let mut conns = Conns::new(mem, ep, &seed, tables.len()).map_err(failed)?;
    let mut dropped = 0;
    let mut reporter = StatusReporter::default();
    loop {
        conns.poll(|state, body| {
            let missing = |table| report_missing(table, &routes.tables, log);
            if let Some((ty, id, bytes)) = state.route(&tables, body, missing) {
                dropped += u64::from(routes.deliver(ty, id, bytes));
            }
        });
        reporter.report(&mut conns, dropped, seq.now(), link_status, log);
        seq.next_cycle().await;
    }
}

/// Logs a fault for the frame port whose table the peer did not announce.
///
/// This usually means the peer was rebuilt with a changed frame type, so its
/// table id no longer matches.
fn report_missing(missing: PacketId, tables: &[(PacketId, String)], log: &mut Log) {
    let Some((_, port)) = tables.iter().find(|(table, _)| *table == missing) else {
        return;
    };
    log.fault(
        "peer_schema_mismatch",
        format!("port `{port}` was built against a table the peer does not announce"),
    );
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
pub(super) struct Routes<'a> {
    /// Each output port.
    ports: Vec<&'a mut Output<Bytes>>,
    /// The packet type and id each port's records arrive under, in port
    /// order.
    keys: Vec<(PacketTy, PacketId)>,
    /// The index of the port for each packet type and id.
    index: HashMap<(PacketTy, PacketId), usize>,
    /// The table id and name of every frame port.
    tables: Vec<(PacketId, String)>,
}

impl<'a> Routes<'a> {
    /// Builds the routing table for `outputs`.
    ///
    /// A message port is keyed by its message id, and a frame port by its
    /// vtable's table id. The definition already rejected two ports with the
    /// same key.
    pub(super) fn of(outputs: &'a mut DynOutputs) -> Self {
        let mut routes = Routes {
            ports: Vec::new(),
            keys: Vec::new(),
            index: HashMap::new(),
            tables: Vec::new(),
        };
        for (index, (def, output)) in outputs.iter_mut().enumerate() {
            let key = (def.schema.packet_ty(), def.schema.packet_id());
            if let RecordSchema::Frame { .. } = def.schema {
                routes.tables.push((key.1, def.name.to_string()));
            }
            routes.keys.push(key);
            routes.index.insert(key, index);
            routes.ports.push(output);
        }
        routes
    }

    /// Returns the ids of the ports with packet type `ty`.
    pub(super) fn ids(&self, ty: PacketTy) -> Vec<PacketId> {
        self.keys
            .iter()
            .filter(|(t, _)| *t == ty)
            .map(|(_, id)| *id)
            .collect()
    }

    /// Writes one record onto the port for its type and id. True if that
    /// port's ring was full.
    pub(super) fn deliver(&mut self, ty: PacketTy, id: PacketId, bytes: &[u8]) -> bool {
        let Some(&index) = self.index.get(&(ty, id)) else {
            return false;
        };
        self.ports[index].write(bytes).is_err()
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{NoWake, RingBuffer};
    use zerocopy::IntoBytes;

    use crate::DefCx;
    use crate::record::{Payload, Record};
    use crate::system::{OutputBinding, PortDef};
    use crate::tests::utils::{Fixed, Imu, Note, ring};

    use super::super::peer::ConnectionState;
    use super::*;

    /// Builds a packet body in its wire format.
    fn body(ty: PacketTy, id: PacketId, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![ty as u8, id[0], id[1], 0];
        body.extend_from_slice(payload);
        body
    }

    fn id_of<R: Record>() -> PacketId {
        R::schema().packet_id()
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

    /// Routes `body` the way a connection that checks no announce does.
    /// True if the port's ring was full.
    fn route(routes: &mut Routes<'_>, body: &[u8]) -> bool {
        let tables = routes.ids(PacketTy::Table);
        let mut state = ConnectionState::new(tables.len());
        state
            .route(&tables, body, |_| {})
            .is_some_and(|(ty, id, bytes)| routes.deliver(ty, id, bytes))
    }

    #[test]
    fn test_route_packet_by_record_id() {
        let rings = [ring::<Fixed>(2, 1), ring::<Note>(2, 1)];
        let mut outputs = ports(&rings);
        let mut routes = Routes::of(&mut outputs);
        assert!(routes.tables.is_empty());
        let mut note = rings[1].view(NoWake).expect("a free slot");
        let hello = body(PacketTy::Msg, id_of::<Note>(), b"hello");
        assert!(!route(&mut routes, &hello));
        assert_eq!(note.drain().next().expect("a record"), Ok(&b"hello"[..]));
    }

    #[test]
    fn test_route_timestamped_message_without_its_timestamp() {
        let rings = [ring::<Fixed>(2, 1), ring::<Note>(2, 1)];
        let mut outputs = ports(&rings);
        let mut routes = Routes::of(&mut outputs);
        let mut note = rings[1].view(NoWake).expect("a free slot");
        let mut stamped = 7i64.to_le_bytes().to_vec();
        stamped.extend_from_slice(b"late");
        let late = body(PacketTy::MsgWithTimestamp, id_of::<Note>(), &stamped);
        assert!(!route(&mut routes, &late));
        assert_eq!(note.drain().next().expect("a record"), Ok(&b"late"[..]));
    }

    #[test]
    fn test_route_table_by_table_id() {
        let rings = [ring::<Fixed>(2, 1), ring::<Imu>(2, 1)];
        let mut outputs = mixed(&rings);
        let mut routes = Routes::of(&mut outputs);
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
        let frame = Imu::new(3, 4.0);
        assert!(!route(
            &mut routes,
            &body(PacketTy::Table, table, frame.as_bytes())
        ));
        assert!(!route(
            &mut routes,
            &body(PacketTy::Table, [9, 9], frame.as_bytes())
        ));
        let received = imu.drain().next().expect("a record").expect("intact");
        assert_eq!(Imu::decode(received), Ok(&frame));
        assert!(imu.drain().next().is_none(), "the unknown table is ignored");
    }

    #[test]
    fn test_ignore_unknown_and_malformed_bodies() {
        let rings = [ring::<Fixed>(2, 1), ring::<Note>(2, 1)];
        let mut outputs = ports(&rings);
        let mut routes = Routes::of(&mut outputs);
        let mut views = rings
            .each_ref()
            .map(|r| r.view(NoWake).expect("a free slot"));
        for junk in [
            body(PacketTy::Msg, [9, 9], b"probe"),
            vec![PacketTy::Msg as u8, 1],
            vec![0xff, 1, 2, 0, 9],
            body(PacketTy::MsgWithTimestamp, id_of::<Note>(), &[0u8; 3]),
        ] {
            assert!(!route(&mut routes, &junk), "{junk:?}");
        }
        assert!(views.iter_mut().all(|v| v.drain().next().is_none()));
    }

    #[test]
    fn test_port_rejects_oversized_record() {
        // Both rings fit the longest record, so only the port's own length
        // limit can reject a long record sent to the shorter port.
        let rings = [ring::<Note>(2, 1), ring::<Note>(2, 1)];
        let mut outputs = ports(&rings);
        let mut routes = Routes::of(&mut outputs);
        let mut fixed = rings[0].view(NoWake).expect("a free slot");
        const { assert!(Fixed::MAX_LEN < 60) };
        assert!(route(
            &mut routes,
            &body(PacketTy::Msg, id_of::<Fixed>(), &[0u8; 60])
        ));
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
    fn test_full_ring_counts_drops() {
        let rings = [ring::<Fixed>(2, 1), ring::<Note>(2, 1)];
        let mut outputs = ports(&rings);
        let mut routes = Routes::of(&mut outputs);
        // Nothing reads the ring, so it fills up and writes start failing.
        let _note = rings[1].view(NoWake).expect("a free slot");
        let filler = body(PacketTy::Msg, id_of::<Note>(), b"filler");
        let full = (0..64).filter(|_| route(&mut routes, &filler)).count();
        assert!(full > 0);
    }
}
