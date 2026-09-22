//! `Publish`: the records a target lists, served to whoever connects.

use std::rc::Rc;

use futures_lite::future;
use metor_fsw_3_ring::Notifier;
use metor_proto::types::{PacketId, Timestamp};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::coordinator::ParamError;
use crate::log::Log;
use crate::port::{DynInputs, Output};
use crate::system::PortDef;
use crate::{Stop, system};

use super::conn::{self, Connections, Event};
use super::inbox::Inbox;
use super::transport::{Endpoint, Transport, source};
use super::wire;
use super::{LinkStatus, default_conn_cap};

/// What a `Publish` is configured with.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishParams {
    pub transport: Transport,
    /// The target's telemetry namespace, which every leaf hangs under.
    #[serde(default)]
    pub namespace: Option<String>,
    /// This link's name, so a subscriber can tell one of a target's links from another.
    #[serde(default)]
    pub link: String,
    /// The size of the per-connection pending write buffer, and of a batch.
    #[serde(default = "default_conn_cap")]
    pub conn_cap: usize,
}

/// Serves every record its config wires into it over metor-proto.
pub struct Publish {
    endpoint: Option<Endpoint>,
    namespace: Option<String>,
    link: String,
    conn_cap: usize,
}

impl Publish {
    /// Binds the transport, so a taken address fails the build.
    pub fn new(params: PublishParams) -> Result<Self, ParamError> {
        let endpoint = params.transport.bind()?;
        super::record_bound(&params.link, &endpoint);
        Ok(Self {
            endpoint: Some(endpoint),
            namespace: params.namespace,
            link: params.link,
            conn_cap: params.conn_cap,
        })
    }
}

#[system]
impl Publish {
    /// Announces its ports to each new connection, then batches their records.
    async fn run(
        &mut self,
        inputs: &mut DynInputs<Notifier>,
        link_status: &mut Output<LinkStatus>,
        log: &mut Log,
        stop: Stop,
    ) {
        let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
        let (blob, ids) = wire::announce(&defs, self.namespace.as_deref(), &self.link);
        report_collisions(log, &defs, &ids);
        // PANIC Safety: `run` is called once, and the endpoint is the bind's.
        let endpoint = self.endpoint.take().expect("one run per instance");
        if let Some(addr) = endpoint.local_addr() {
            log.info(format!("listening on {addr}"));
        }
        let inbox = Rc::new(Inbox::blackhole());
        let conns = Rc::new(Connections::new(
            endpoint.slots(),
            self.conn_cap,
            blob,
            inbox,
        ));
        let _source =
            stellarator::spawn(source(endpoint, conns.clone(), stop.clone())).drop_guard();
        let mut batch = Vec::with_capacity(self.conn_cap);
        let mut reported = LinkStatus::new(Timestamp(0), Default::default(), 0);
        loop {
            let input_ready = async {
                inputs.any_ready().await;
                Event::Ready
            };
            let event = future::or(conn::next(&conns, &stop), input_ready).await;
            match event {
                Event::Ready => {
                    batch.clear();
                    fill(&mut batch, inputs, &ids, self.conn_cap);
                    if !batch.is_empty() {
                        conns.enqueue(&batch);
                    }
                }
                Event::Changed => {}
                Event::Stop => return,
            }
            let status = LinkStatus::new(Timestamp::now(), conns.stats(), 0);
            if status.changed(&reported) {
                let _ = link_status.write(&status);
                reported = status;
            }
        }
    }
}

/// Faults every port that announces a table an earlier port already did.
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

/// Appends a packet per record
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
    use metor_fsw_3_ring::{Config, RingBuffer};
    use metor_proto::types::PacketTy;
    use zerocopy::IntoBytes;

    use crate::link::DEFAULT_CONN_CAP;
    use crate::port::{Input, ring_capacity};
    use crate::record::Record;
    use crate::system::{InputBinding, SystemInputs};
    use crate::tests::utils::{Fixed, Imu};

    use super::*;

    /// The framed length of one `Imu` and one `Fixed` record.
    const IMU_LEN: usize = wire::PACKET_OVERHEAD + Imu::MAX_LEN;
    // Postcard's varint for `a` plus eight bytes of `b`.
    const FIXED_LEN: usize = wire::PACKET_OVERHEAD + 9;

    fn ring<R: Record>() -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(R::MAX_LEN, 8).expect("valid capacity"),
            max_readers: 2,
        })
    }

    /// Two ports, an `Imu` frame and a `Fixed` message, over their own rings.
    fn ports(rings: &[RingBuffer; 2]) -> DynInputs<Notifier> {
        let wake = Notifier::default();
        let defs = [
            Input::<Imu>::def("plant.imu"),
            Input::<Fixed>::def("cmds.fixed"),
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

    /// One record on each port, as their producers would write them.
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
        let rings = [ring::<Imu>(), ring::<Fixed>()];
        let mut inputs = ports(&rings);
        let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
        let (_, ids) = wire::announce(&defs, Some("cube_sat"), "pub");
        produce(&rings, 4.0);

        let mut batch = Vec::new();
        fill(&mut batch, &mut inputs, &ids, DEFAULT_CONN_CAP);
        let (table, msg) = (ids[0].expect("a table id"), ids[1].expect("a message id"));
        assert_eq!(batch.len(), IMU_LEN + FIXED_LEN);
        assert_eq!(&batch[4..5], &[PacketTy::Table as u8]);
        assert_eq!(&batch[5..7], &table);
        assert_eq!(&batch[IMU_LEN + 4..IMU_LEN + 5], &[PacketTy::Msg as u8]);
        assert_eq!(&batch[IMU_LEN + 5..IMU_LEN + 7], &msg);
    }

    #[test]
    fn test_drain_colliding_port() {
        let rings = [ring::<Imu>(), ring::<Fixed>()];
        let mut inputs = ports(&rings);
        produce(&rings, 1.0);
        let mut batch = Vec::new();
        fill(&mut batch, &mut inputs, &[None, None], DEFAULT_CONN_CAP);
        assert!(batch.is_empty());
        produce(&rings, 2.0);
        fill(&mut batch, &mut inputs, &[None, None], DEFAULT_CONN_CAP);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_stalled_mirror_fills_to_cap_over_several_batches() {
        let rings = [ring::<Imu>(), ring::<Fixed>()];
        let mut inputs = ports(&rings);
        let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
        let (_, ids) = wire::announce(&defs, None, "pub");
        for cycle in 0..4 {
            produce(&rings, cycle as f64);
        }
        // Room for three `Imu` records, so what waits takes several fills.
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
        let rings = [ring::<Imu>(), ring::<Fixed>()];
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
