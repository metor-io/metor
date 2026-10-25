//! Encodes a cycle's records once, as the batch every connection sends.

use metor_proto::types::{PacketId, PacketTy};

use crate::io::Buf;
use crate::port::{DynInputs, InputWake};

use super::wire::{self, PACKET_OVERHEAD};

/// The packet ids of a link's inputs, in port order, and the buffer a
/// batch is encoded in.
pub(crate) struct Batcher {
    ids: Box<[PacketId]>,
    scratch: Buf,
}

impl Batcher {
    pub(crate) fn new(ids: impl Into<Box<[PacketId]>>, scratch: Buf) -> Self {
        Self {
            ids: ids.into(),
            scratch,
        }
    }

    /// Encodes one packet per waiting record, stopping before the batch
    /// would outgrow the scratch buffer. `None` if no record was waiting,
    /// or the buffer is from an earlier make. A record longer than its port
    /// allows is dropped.
    pub(crate) fn fill<W: InputWake>(&mut self, inputs: &mut DynInputs<W>) -> Option<&[u8]> {
        self.scratch.clear();
        if !self.scratch.live() {
            return None;
        }
        for ((def, input), id) in inputs.iter_mut().zip(&self.ids) {
            let (ty, room) = (def.schema.packet_ty(), def.max_len + PACKET_OVERHEAD);
            let mut records = input.drain();
            while room <= self.scratch.spare() {
                match records.next() {
                    Some(Ok(bytes)) => append(&mut self.scratch, ty, *id, bytes),
                    Some(Err(_)) => {}
                    None => break,
                }
            }
        }
        (!self.scratch.is_empty()).then_some(&*self.scratch)
    }
}

/// Appends one packet to `buf`, or nothing if it does not fit.
fn append(buf: &mut Buf, ty: PacketTy, id: PacketId, payload: &[u8]) {
    if PACKET_OVERHEAD + payload.len() <= buf.spare() {
        buf.extend(&wire::packet_head(ty, id, payload.len()));
        buf.extend(payload);
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Notifier, RingBuffer};
    use zerocopy::IntoBytes;

    use crate::DefCx;
    use crate::io::Stream;
    use crate::io::test::{TestIo, cap};
    use crate::port::Input;
    use crate::record::{Payload, Record};
    use crate::system::{InputBinding, PortDef};
    use crate::tests::utils::{Fixed, Imu, ring};

    use super::*;

    /// The length of one `Imu` record once framed as a packet.
    const IMU_LEN: usize = PACKET_OVERHEAD + Imu::MAX_LEN;
    // The framed length of one `Fixed` record: postcard's varint for `a`
    // plus eight bytes of `b`.
    const FIXED_LEN: usize = PACKET_OVERHEAD + 9;

    /// Two input ports, an `Imu` frame port and a `Fixed` message port,
    /// each reading its own ring.
    fn ports(rings: &[RingBuffer; 2]) -> DynInputs<Notifier> {
        let wake = Notifier::default();
        let defs = [
            Input::<Imu>::def(&DefCx::empty(), "plant.imu"),
            Input::<Fixed>::def(&DefCx::empty(), "cmds.fixed"),
        ];
        DynInputs::new(
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

    /// A batcher with `cap` bytes of scratch, over the ports' ids.
    fn batcher(inputs: &mut DynInputs<Notifier>, cap_bytes: usize) -> (Batcher, TestIo) {
        let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
        let (_, ids) = wire::announce(&defs, Some("cube_sat"), "pub");
        let (_, io) = cap::<Stream>();
        let scratch = io.make(|init| init.alloc(cap_bytes));
        (Batcher::new(ids, scratch), io)
    }

    #[test]
    fn test_batch_encodes_records() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let (mut batcher, _io) = batcher(&mut inputs, 1024);
        let (table, msg) = (batcher.ids[0], batcher.ids[1]);
        produce(&rings, 4.0);
        let batch = batcher.fill(&mut inputs).expect("two records");
        assert_eq!(batch.len(), IMU_LEN + FIXED_LEN);
        assert_eq!(&batch[4..7], &[PacketTy::Table as u8, table[0], table[1]]);
        let msg_head = &batch[IMU_LEN + 4..IMU_LEN + 7];
        assert_eq!(msg_head, &[PacketTy::Msg as u8, msg[0], msg[1]]);
        assert_eq!(
            &batch[..4],
            &((IMU_LEN - 4) as u32).to_le_bytes(),
            "the prefix counts the header and payload"
        );
    }

    #[test]
    fn test_nothing_waiting_is_no_batch() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let (mut batcher, _io) = batcher(&mut inputs, 1024);
        assert!(batcher.fill(&mut inputs).is_none());
        produce(&rings, 1.0);
        assert!(batcher.fill(&mut inputs).is_some());
        assert!(batcher.fill(&mut inputs).is_none(), "each record goes once");
    }

    #[test]
    fn test_stalled_mirror_fills_to_cap_over_several_batches() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        for cycle in 0..4 {
            produce(&rings, f64::from(cycle));
        }
        // Room for three `Imu` records, so draining everything takes several
        // fills.
        let cap_bytes = 3 * IMU_LEN + FIXED_LEN - 1;
        let (mut batcher, _io) = batcher(&mut inputs, cap_bytes);
        let mut lengths = Vec::new();
        while let Some(batch) = batcher.fill(&mut inputs) {
            assert!(batch.len() <= cap_bytes);
            lengths.push(batch.len());
        }
        assert_eq!(lengths[0], 3 * IMU_LEN);
        assert!(lengths.len() > 1, "{lengths:?}");
        assert_eq!(lengths.iter().sum::<usize>(), 4 * (IMU_LEN + FIXED_LEN));
    }

    #[test]
    fn test_record_past_the_room_left_is_dropped_whole() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let room = PACKET_OVERHEAD + Fixed::MAX_LEN;
        let (mut batcher, _io) = batcher(&mut inputs, IMU_LEN + room);
        let long = [0u8; Fixed::MAX_LEN + 8];
        rings[1]
            .writer(Notifier::default())
            .expect("a free writer")
            .try_write(&long)
            .expect("ring has room");
        produce(&rings, 1.0);
        let batch = batcher.fill(&mut inputs).expect("the records that fit");
        assert_eq!(batch.len(), IMU_LEN + FIXED_LEN, "no truncated packet");
    }

    #[test]
    fn test_remade_scratch_leaves_records_waiting() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let (mut batcher, io) = batcher(&mut inputs, 1024);
        produce(&rings, 1.0);
        io.make(|init| init.alloc(1024));
        assert!(batcher.fill(&mut inputs).is_none());
        let fresh = io.make(|init| init.alloc(1024));
        batcher.scratch = fresh;
        assert!(batcher.fill(&mut inputs).is_some(), "nothing was drained");
    }

    #[test]
    fn test_scratch_too_small_for_any_record_sends_nothing() {
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let (mut batcher, _io) = batcher(&mut inputs, FIXED_LEN - 1);
        produce(&rings, 1.0);
        assert!(batcher.fill(&mut inputs).is_none());
    }
}
