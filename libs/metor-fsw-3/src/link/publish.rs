//! `Publish`, the link that sends a target's records to its peers.

use std::rc::Rc;

use core::ops::{Deref, DerefMut};

use futures_lite::future;
use metor_fsw_3_ring::Notifier;
use metor_proto::types::{PacketId, table_id};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError};
use crate::fn_system::{Cycle, Defs, Param, ParamNames, Views, Writers};
use crate::log::Log;
use crate::port::{DynInputs, InputWake, Output};
use crate::record::RecordSchema;
use crate::system::PortDef;
use crate::{Stop, system};

use super::conn::{ConnCap, Connections};
use super::inbox::Inbox;
use super::transport::Endpoint;
use super::wire;
use super::{LinkParams, LinkStatus, StatusReporter};

/// The configuration for a [`Publish`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishParams {
    #[serde(flatten)]
    pub link: LinkParams,
    /// The size in bytes of each connection's pending write buffer. It is
    /// also the largest batch the link builds.
    #[serde(default)]
    pub conn_cap: ConnCap,
}

/// A link that sends every record wired into it to its peers over
/// metor-proto.
pub struct Publish {
    endpoint: Option<Endpoint>,
    params: PublishParams,
}

impl Publish {
    /// Binds the transport. This happens at build time, so a taken address
    /// fails the build.
    pub fn new(params: PublishParams) -> Result<Self, ParamError> {
        let endpoint = params.link.transport.bind()?;
        Ok(Self {
            endpoint: Some(endpoint),
            params,
        })
    }
}

/// Runs the `Publish` link.
///
/// Each new connection is first sent the announce, which describes every
/// input port's schema. After that, the link collects the records waiting on
/// its inputs into a batch and queues the batch on every connection.
#[system]
pub async fn publish(
    #[state] s: &mut Publish,
    inputs: &mut PublishInputs,
    link_status: &mut Output<LinkStatus>,
    log: &mut Log,
    stop: Stop,
) {
    let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
    let PublishParams {
        link: LinkParams {
            namespace, link, ..
        },
        conn_cap,
    } = &s.params;
    let (blob, ids) = wire::announce(&defs, namespace.as_deref(), link);
    // PANIC Safety: `new` always sets the endpoint, and a system runs once.
    let endpoint = s.endpoint.take().expect("one run per instance");
    if let Some(addr) = endpoint.local_addr() {
        log.info(format!("listening on {addr}"));
    }
    let inbox = Rc::new(Inbox::blackhole());
    let (mut conns, serving) = Connections::new(endpoint.slots(), *conn_cap, blob, inbox);
    let _source = stellarator::spawn(endpoint.run(serving, stop.clone())).drop_guard();
    let mut batch = Vec::with_capacity(conn_cap.bytes());
    let mut reporter = StatusReporter::default();
    let run = async {
        loop {
            future::or(inputs.any_ready(), conns.changed()).await;
            batch.clear();
            fill(&mut batch, inputs, &ids, conn_cap.bytes());
            if !batch.is_empty() {
                conns.enqueue(&batch);
            }
            reporter.report(LinkStatus::new(conns.stats(), 0), link_status, log);
        }
    };
    future::or(run, stop.wait()).await;
}

/// A `Publish`'s inputs: one dynamic port per wired output.
///
/// A record is sent under its frame's table id, so the definition rejects two
/// frame ports whose tables share an id.
pub struct PublishInputs<W: InputWake = Notifier>(DynInputs<W>);

impl<W: InputWake> Deref for PublishInputs<W> {
    type Target = DynInputs<W>;
    fn deref(&self) -> &DynInputs<W> {
        &self.0
    }
}

impl<W: InputWake> DerefMut for PublishInputs<W> {
    fn deref_mut(&mut self) -> &mut DynInputs<W> {
        &mut self.0
    }
}

impl<D: InputWake> Param for PublishInputs<D> {
    type In<W: InputWake> = PublishInputs<W>;
    type Out = ();
    type Item<'a, W: InputWake> = &'a mut PublishInputs<W>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        let first = defs.inputs.len();
        <DynInputs<D> as Param>::append_defs(names, cx, defs)?;
        check_table_ids(&defs.inputs[first..])
    }

    fn bind_in<W: InputWake>(views: &mut Views<W>) -> PublishInputs<W> {
        PublishInputs(<DynInputs<D> as Param>::bind_in(views))
    }

    fn bind_out(names: &mut ParamNames<'_>, writers: &mut Writers) -> Result<(), ParamError> {
        <DynInputs<D> as Param>::bind_out(names, writers)
    }

    fn get<'a, W: InputWake>(
        input: &'a mut PublishInputs<W>,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> &'a mut PublishInputs<W> {
        input
    }
}

/// Rejects two frame ports whose tables share an id.
fn check_table_ids(ports: &[PortDef]) -> Result<(), DefError> {
    let mut seen: Vec<(PacketId, &str)> = Vec::with_capacity(ports.len());
    for port in ports {
        let RecordSchema::Frame { vtable, .. } = &port.schema else {
            continue;
        };
        let id = table_id(vtable);
        if let Some((_, other)) = seen.iter().find(|(known, _)| *known == id) {
            return Err(DefError::TableIdCollision {
                port: port.name.to_string(),
                other: other.to_string(),
            });
        }
        seen.push((id, &port.name));
    }
    Ok(())
}

/// Appends one packet per waiting record to `batch`, stopping before `batch`
/// would grow past `cap`.
fn fill(batch: &mut Vec<u8>, inputs: &mut DynInputs<Notifier>, ids: &[PacketId], cap: usize) {
    for ((def, input), id) in inputs.iter_mut().zip(ids) {
        let (ty, room) = (def.schema.packet_ty(), def.max_len + wire::PACKET_OVERHEAD);
        let mut records = input.drain();
        while batch.len() + room <= cap {
            match records.next() {
                Some(Ok(bytes)) => wire::append_packet(batch, ty, *id, bytes),
                Some(Err(_)) => {}
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::RingBuffer;
    use metor_proto::types::PacketTy;
    use zerocopy::IntoBytes;

    use crate::DefCx;
    use crate::port::Input;
    use crate::record::{Payload, Record};
    use crate::system::{InputBinding, Ports};
    use crate::tests::utils::{Fixed, Imu, ring};

    use super::*;

    /// The length of one `Imu` record once framed as a packet.
    const IMU_LEN: usize = wire::PACKET_OVERHEAD + Imu::MAX_LEN;
    // The framed length of one `Fixed` record: postcard's varint for `a`
    // plus eight bytes of `b`.
    const FIXED_LEN: usize = wire::PACKET_OVERHEAD + 9;

    /// Builds two input ports, an `Imu` frame port and a `Fixed` message
    /// port, each reading its own ring.
    fn ports(rings: &[RingBuffer; 2]) -> DynInputs<Notifier> {
        let wake = Notifier::default();
        let defs = [
            Input::<Imu>::def(&DefCx::empty(), "plant.imu"),
            Input::<Fixed>::def(&DefCx::empty(), "cmds.fixed"),
        ];
        DynInputs::bind(
            defs.into_iter()
                .zip(rings)
                .map(|(def, ring)| InputBinding {
                    def,
                    views: vec![ring.view(wake.clone()).expect("a free slot")],
                })
                .collect(),
        )
    }

    /// Writes one record to each ring, the way each port's producer would.
    fn produce(rings: &[RingBuffer; 2], sample: f64) {
        let imu = Imu::new(1, sample);
        rings[0]
            .writer(Notifier::default())
            .expect("a free writer")
            .try_write(imu.as_bytes())
            .expect("ring has room");
        let mut buf = [0u8; Fixed::MAX_LEN];
        let fixed = Fixed { a: 1, b: sample };
        let bytes = fixed.encode(&mut buf).expect("encodes");
        rings[1]
            .writer(Notifier::default())
            .expect("a free writer")
            .try_write(bytes)
            .expect("ring has room");
    }

    #[test]
    fn test_batch_encodes_records() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
        let (_, ids) = wire::announce(&defs, Some("cube_sat"), "pub");
        produce(&rings, 4.0);

        let mut batch = Vec::new();
        fill(&mut batch, &mut inputs, &ids, ConnCap::default().bytes());
        let (table, msg) = (ids[0], ids[1]);
        assert_eq!(batch.len(), IMU_LEN + FIXED_LEN);
        assert_eq!(&batch[4..5], &[PacketTy::Table as u8]);
        assert_eq!(&batch[5..7], &table);
        assert_eq!(&batch[IMU_LEN + 4..IMU_LEN + 5], &[PacketTy::Msg as u8]);
        assert_eq!(&batch[IMU_LEN + 5..IMU_LEN + 7], &msg);
    }

    #[test]
    fn test_reject_colliding_tables() {
        let imu = Input::<Imu>::def(&DefCx::empty(), "plant.imu");
        let fixed = Input::<Fixed>::def(&DefCx::empty(), "cmds.fixed");
        let twin = PortDef {
            name: "copy.imu".into(),
            ..imu.clone()
        };
        assert_eq!(check_table_ids(&[imu.clone(), fixed.clone()]), Ok(()));
        assert_eq!(
            check_table_ids(&[imu, fixed.clone(), twin, fixed]),
            Err(DefError::TableIdCollision {
                port: "copy.imu".into(),
                other: "plant.imu".into(),
            }),
            "messages may share an id; tables may not"
        );
    }

    #[test]
    fn test_stalled_mirror_fills_to_cap_over_several_batches() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
        let (_, ids) = wire::announce(&defs, None, "pub");
        for cycle in 0..4 {
            produce(&rings, cycle as f64);
        }
        // Room for three `Imu` records, so draining everything takes several
        // fills.
        let cap = 3 * IMU_LEN + FIXED_LEN - 1;
        let mut batch = Vec::with_capacity(cap);
        let mut lengths = Vec::new();
        loop {
            batch.clear();
            fill(&mut batch, &mut inputs, &ids, cap);
            assert!(batch.len() <= cap && batch.capacity() == cap);
            if batch.is_empty() {
                break;
            }
            lengths.push(batch.len());
        }
        assert_eq!(lengths[0], 3 * IMU_LEN);
        assert!(lengths.len() > 1, "{lengths:?}");
        assert_eq!(lengths.iter().sum::<usize>(), 4 * (IMU_LEN + FIXED_LEN));
    }

    #[test]
    fn test_steady_batch_preserves_capacity() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
        let (_, ids) = wire::announce(&defs, None, "pub");
        let cap = 4 * (IMU_LEN + FIXED_LEN);
        let mut batch = Vec::with_capacity(cap);
        for cycle in 0..1_000 {
            batch.clear();
            produce(&rings, cycle as f64);
            fill(&mut batch, &mut inputs, &ids, cap);
            assert_eq!(batch.len(), IMU_LEN + FIXED_LEN);
        }
        assert_eq!(batch.capacity(), cap);
    }
}
