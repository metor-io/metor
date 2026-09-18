//! `Publish`: the records a target lists, served to whoever connects.

use metor_fsw_3_ring::Notifier;
use metor_proto::types::Timestamp;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::coordinator::ParamError;
use crate::log::Log;
use crate::port::{DynInputs, Output};
use crate::system::PortDef;
use crate::{Stop, system};

use super::conn::Connections;
use super::transport::{Endpoint, Transport, incoming};
use super::wire::{self, Wire};
use super::{LinkStatus, pending_cap};

/// Batches one port holds before a slow connection drops one, in records.
const BATCH_DEPTH: usize = 8;

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
    #[serde(default = "pending_cap")]
    pub pending_cap: usize,
}

/// Serves every record its config wires into it over metor-proto.
pub struct Publish {
    endpoint: Option<Endpoint>,
    namespace: Option<String>,
    link: String,
    pending_cap: usize,
}

impl Publish {
    /// Binds the transport, so a taken address fails the build.
    pub fn new(params: PublishParams) -> Result<Self, ParamError> {
        Ok(Self {
            endpoint: Some(params.transport.bind()?),
            namespace: params.namespace,
            link: params.link,
            pending_cap: params.pending_cap,
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
        let (blob, wire) = wire::announce(&defs, self.namespace.as_deref(), &self.link);
        report_collisions(log, &defs, &wire);
        // PANIC Safety: `run` is called once, and the endpoint is the bind's.
        let endpoint = self.endpoint.take().expect("one run per instance");
        if let Some(addr) = endpoint.local_addr() {
            log.info(format!("listening on {addr}"));
        }
        let mut conns = Connections::new(endpoint.slots(), self.pending_cap);
        let (incoming, _source) = incoming(endpoint, stop.clone());
        incoming.want();
        let mut batch = Vec::with_capacity(batch_cap(&defs));
        let mut reported = LinkStatus::new(Timestamp(0), Default::default(), 0);
        loop {
            match next(&incoming, inputs, &stop).await {
                Event::Connected(stream) => {
                    conns.open(stream, blob.clone(), |_| {});
                }
                Event::Ready => {
                    batch.clear();
                    fill(&mut batch, inputs, &wire);
                    if !batch.is_empty() {
                        conns.enqueue(&batch);
                    }
                }
                Event::Stop => return,
            }
            conns.prune();
            if conns.has_free() {
                incoming.want();
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
fn report_collisions(log: &mut Log, defs: &[PortDef], wire: &[Option<Wire>]) {
    for (def, _) in defs.iter().zip(wire).filter(|(_, w)| w.is_none()) {
        log.fault(
            "table_id_collision",
            format!(
                "port `{}` announces a table another port already did",
                def.name
            ),
        );
    }
}

/// What woke the link's loop.
enum Event {
    Connected(stellarator::net::TcpStream),
    Ready,
    Stop,
}

/// The first of a connection, a record, and the stop.
async fn next(
    incoming: &super::transport::Incoming,
    inputs: &mut DynInputs<Notifier>,
    stop: &Stop,
) -> Event {
    let connected = async { Event::Connected(incoming.next().await) };
    let ready = async {
        inputs.any_ready().await;
        Event::Ready
    };
    let stopped = async {
        stop.wait().await;
        Event::Stop
    };
    futures_lite::future::or(futures_lite::future::or(connected, ready), stopped).await
}

/// Appends one packet per waiting record, in port order.
fn fill(batch: &mut Vec<u8>, inputs: &mut DynInputs<Notifier>, wire: &[Option<Wire>]) {
    for ((_, input), wire) in inputs.iter_mut().zip(wire) {
        for bytes in input.drain() {
            let (Ok(bytes), Some(wire)) = (bytes, wire) else {
                continue;
            };
            wire::append_packet(batch, wire.ty(), wire.id(), bytes);
        }
    }
}

/// Room for `BATCH_DEPTH` cycles of every port, so a steady batch never grows.
fn batch_cap(defs: &[PortDef]) -> usize {
    defs.iter()
        .map(|def| (def.max_len + wire::PACKET_OVERHEAD) * def.depth * BATCH_DEPTH)
        .sum()
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Config, RingBuffer};
    use metor_proto::types::PacketTy;
    use zerocopy::IntoBytes;

    use crate::port::{Input, ring_capacity};
    use crate::record::Record;
    use crate::system::{InputBinding, SystemInputs};
    use crate::tests::utils::{Fixed, Imu};

    use super::*;

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
    fn a_batch_is_one_packet_per_record_under_its_ports_wire_id() {
        let rings = [ring::<Imu>(), ring::<Fixed>()];
        let mut inputs = ports(&rings);
        let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
        let (_, wire) = wire::announce(&defs, Some("cube_sat"), "pub");
        produce(&rings, 4.0);

        let mut batch = Vec::new();
        fill(&mut batch, &mut inputs, &wire);
        let (table, msg) = (
            wire[0].expect("a frame wire"),
            wire[1].expect("a message wire"),
        );
        let table_len = wire::PACKET_OVERHEAD + Imu::MAX_LEN;
        // Postcard's varint for `a` plus eight bytes of `b`.
        let msg_len = wire::PACKET_OVERHEAD + 9;
        assert_eq!(batch.len(), table_len + msg_len);
        assert_eq!(&batch[4..5], &[PacketTy::Table as u8]);
        assert_eq!(&batch[5..7], &table.id());
        assert_eq!(&batch[table_len + 4..table_len + 5], &[PacketTy::Msg as u8]);
        assert_eq!(&batch[table_len + 5..table_len + 7], &msg.id());
    }

    #[test]
    fn a_port_whose_table_collided_carries_nothing_and_is_still_drained() {
        let rings = [ring::<Imu>(), ring::<Fixed>()];
        let mut inputs = ports(&rings);
        produce(&rings, 1.0);
        let mut batch = Vec::new();
        fill(&mut batch, &mut inputs, &[None, None]);
        assert!(batch.is_empty());
        produce(&rings, 2.0);
        fill(&mut batch, &mut inputs, &[None, None]);
        assert!(batch.is_empty());
    }

    #[test]
    fn a_steady_batch_never_outgrows_its_reservation() {
        let rings = [ring::<Imu>(), ring::<Fixed>()];
        let mut inputs = ports(&rings);
        let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
        let (_, wire) = wire::announce(&defs, None, "pub");
        let mut batch = Vec::with_capacity(batch_cap(&defs));
        let reserved = batch.capacity();
        for cycle in 0..1_000 {
            batch.clear();
            produce(&rings, cycle as f64);
            fill(&mut batch, &mut inputs, &wire);
            assert!(!batch.is_empty());
        }
        assert_eq!(batch.capacity(), reserved);
    }
}
