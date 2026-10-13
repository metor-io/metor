//! `Publish`, the link that sends a target's records to its peers.

use std::rc::Rc;

use futures_lite::future;
use metor_fsw_3_ring::Notifier;
use metor_proto::types::PacketId;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::coordinator::ParamError;
use crate::log::Log;
use crate::port::{DynInputs, Output};
use crate::system::PortDef;
use crate::{Stop, system};

use super::conn::{self, Connections, Event};
use super::inbox::Inbox;
use super::transport::Endpoint;
use super::wire;
use super::{LinkParams, LinkStatus, default_conn_cap, report};

/// The configuration for a [`Publish`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishParams {
    #[serde(flatten)]
    pub link: LinkParams,
    /// The size in bytes of each connection's pending write buffer. It is
    /// also the largest batch the link builds.
    #[serde(default = "default_conn_cap")]
    pub conn_cap: usize,
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
        super::record_bound(&params.link.link, &endpoint);
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
    inputs: &mut DynInputs<Notifier>,
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
    report_collisions(log, &defs, &ids);
    // PANIC Safety: `new` always sets the endpoint, and a system runs once.
    let endpoint = s.endpoint.take().expect("one run per instance");
    if let Some(addr) = endpoint.local_addr() {
        log.info(format!("listening on {addr}"));
    }
    let inbox = Rc::new(Inbox::blackhole());
    let conns = Rc::new(Connections::new(endpoint.slots(), *conn_cap, blob, inbox));
    let _source = stellarator::spawn(endpoint.run(conns.clone(), stop.clone())).drop_guard();
    let mut batch = Vec::with_capacity(*conn_cap);
    let mut reported = LinkStatus::default();
    loop {
        let input_ready = async {
            inputs.any_ready().await;
            Event::Ready
        };
        let event = future::or(conn::next(&conns, &stop), input_ready).await;
        match event {
            Event::Ready => {
                batch.clear();
                fill(&mut batch, inputs, &ids, *conn_cap);
                if !batch.is_empty() {
                    conns.enqueue(&batch);
                }
            }
            Event::Changed => {}
            Event::Stop => return,
        }
        let status = LinkStatus::new(conns.stats(), 0);
        report(&status, &mut reported, link_status, log);
    }
}

/// Logs a fault for every port whose table id was already announced by an
/// earlier port.
fn report_collisions(log: &mut Log, defs: &[PortDef], ids: &[Option<PacketId>]) {
    for (def, _) in defs.iter().zip(ids).filter(|(_, id)| id.is_none()) {
        log.fault(
            "table_id_collision",
            format!(
                "port `{}` announces a table another port already did",
                def.name
            ),
        );
    }
}

/// Appends one packet per waiting record to `batch`, stopping before `batch`
/// would grow past `cap`.
///
/// Records on a port with no id (a table id collision) are drained and
/// dropped.
fn fill(
    batch: &mut Vec<u8>,
    inputs: &mut DynInputs<Notifier>,
    ids: &[Option<PacketId>],
    cap: usize,
) {
    for ((def, input), id) in inputs.iter_mut().zip(ids) {
        let Some(id) = id else {
            input.drain().for_each(drop);
            continue;
        };
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
    use crate::record::Record;
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
        fill(&mut batch, &mut inputs, &ids, default_conn_cap());
        let (table, msg) = (ids[0].expect("a table id"), ids[1].expect("a message id"));
        assert_eq!(batch.len(), IMU_LEN + FIXED_LEN);
        assert_eq!(&batch[4..5], &[PacketTy::Table as u8]);
        assert_eq!(&batch[5..7], &table);
        assert_eq!(&batch[IMU_LEN + 4..IMU_LEN + 5], &[PacketTy::Msg as u8]);
        assert_eq!(&batch[IMU_LEN + 5..IMU_LEN + 7], &msg);
    }

    #[test]
    fn test_drain_colliding_port() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        produce(&rings, 1.0);
        let mut batch = Vec::new();
        fill(&mut batch, &mut inputs, &[None, None], default_conn_cap());
        assert!(batch.is_empty());
        produce(&rings, 2.0);
        fill(&mut batch, &mut inputs, &[None, None], default_conn_cap());
        assert!(batch.is_empty());
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
