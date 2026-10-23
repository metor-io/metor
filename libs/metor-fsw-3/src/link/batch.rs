//! Encodes a cycle's records once, as the batch every connection sends.

use metor_proto::types::{PacketId, PacketTy};

use crate::io::Buf;
use crate::port::{DynInputs, Input, InputWake};
use crate::record::Bytes;
use crate::system::PortDef;

use super::wire::{self, PACKET_OVERHEAD};

/// The packet ids of a link's inputs, in port order, and the buffer a
/// batch is encoded in.
pub(crate) struct Batcher {
    ids: Box<[PacketId]>,
    scratch: Buf,
    /// The port the next batch starts at.
    next: usize,
}

impl Batcher {
    pub(crate) fn new(ids: impl Into<Box<[PacketId]>>, scratch: Buf) -> Self {
        Self {
            ids: ids.into(),
            scratch,
            next: 0,
        }
    }

    /// Encodes one packet per waiting record, stopping before the batch
    /// would outgrow the scratch buffer. `None` if no record was waiting,
    /// or the buffer is from an earlier make. A record longer than its port
    /// allows is dropped.
    ///
    /// The next batch starts at the first port past this one's start that
    /// it left records on, else at the port after its start. So a port that
    /// fills every batch crowds out none of the others.
    pub(crate) fn fill<W: InputWake>(&mut self, inputs: &mut DynInputs<W>) -> Option<&[u8]> {
        self.scratch.clear();
        if !self.scratch.live() {
            return None;
        }
        let start = self.next;
        let mut left = None;
        for (index, def, input) in inputs.iter_mut_from(start) {
            let Some(id) = self.ids.get(index) else {
                continue;
            };
            let drained = encode(&mut self.scratch, def, *id, input);
            left = left.or((!drained && index != start).then_some(index));
        }
        let wrap = self.ids.len().max(1);
        self.next = left.unwrap_or((start + 1) % wrap);
        (!self.scratch.is_empty()).then_some(&*self.scratch)
    }
}

/// Encodes `input`'s waiting records into `buf` while one more fits. True if
/// none is left waiting.
fn encode<W: InputWake>(
    buf: &mut Buf,
    def: &PortDef,
    id: PacketId,
    input: &mut Input<Bytes, W>,
) -> bool {
    let (ty, room) = (def.schema.packet_ty(), def.max_len + PACKET_OVERHEAD);
    let mut records = input.drain();
    while room <= buf.spare() {
        match records.next() {
            Some(Ok(bytes)) => append(buf, ty, id, bytes),
            Some(Err(_)) => {}
            None => return true,
        }
    }
    drop(records);
    !input.has_record()
}

/// Appends one packet to `buf`, or nothing if it does not fit.
pub(super) fn append(buf: &mut Buf, ty: PacketTy, id: PacketId, payload: &[u8]) {
    if PACKET_OVERHEAD + payload.len() <= buf.spare() {
        buf.extend(&wire::packet_head(ty, id, payload.len()));
        buf.extend(payload);
    }
}

#[cfg(test)]
mod tests {
    use core::ops::Range;

    use metor_fsw_3_ring::{Notifier, RingBuffer};
    use zerocopy::IntoBytes;

    use crate::DefCx;
    use crate::io::Stream;
    use crate::io::test::{TestIo, cap};
    use crate::port::Input;
    use crate::record::{Payload, Record};
    use crate::system::{InputBinding, PortDef};
    use crate::tests::utils::{Fixed, Imu, ring, ring_of};

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

    /// Message ports of `max_len` bytes each, one ring of `depth` records
    /// apiece.
    fn sized_ports(max_lens: &[usize], depth: usize) -> (Vec<RingBuffer>, DynInputs<Notifier>) {
        let wake = Notifier::default();
        let rings: Vec<_> = max_lens.iter().map(|&len| ring_of(len, depth, 1)).collect();
        let bindings = max_lens.iter().zip(&rings).enumerate();
        let bindings = bindings.map(|(index, (&max_len, ring))| InputBinding {
            def: PortDef {
                name: format!("port{index}").into(),
                max_len,
                ..Input::<Fixed>::def(&DefCx::empty(), "port")
            },
            views: vec![ring.view(wake.clone()).expect("a free slot")],
        });
        let inputs = DynInputs::new(bindings.collect());
        (rings, inputs)
    }

    /// Writes one `len`-byte record per count in `counts` to port `port`'s
    /// ring. A record starts with the port, then its count.
    fn write(rings: &[RingBuffer], port: u8, counts: Range<u16>, len: usize) {
        let ring = &rings[usize::from(port)];
        let mut writer = ring.writer(Notifier::default()).expect("a free writer");
        for count in counts {
            let mut record = vec![0; len];
            record[0] = port;
            record[1..3].copy_from_slice(&count.to_le_bytes());
            writer.try_write(&record).expect("ring has room");
        }
    }

    /// The port and count of each packet in `batch`.
    fn sent(batch: &[u8]) -> Vec<(u8, u16)> {
        let mut packets = Vec::new();
        let mut rest = batch;
        while let Some((prefix, tail)) = rest.split_first_chunk::<4>() {
            let len = u32::from_le_bytes(*prefix) as usize;
            let payload = &tail[PACKET_OVERHEAD - 4..len];
            packets.push((payload[0], u16::from_le_bytes([payload[1], payload[2]])));
            rest = &tail[len..];
        }
        packets
    }

    /// Fills one batch per cycle after `produce`, and returns each batch's
    /// packets.
    fn run(
        inputs: &mut DynInputs<Notifier>,
        batcher: &mut Batcher,
        cycles: u16,
        mut produce: impl FnMut(u16),
    ) -> Vec<Vec<(u8, u16)>> {
        let batches = (0..cycles).map(|cycle| {
            produce(cycle);
            batcher.fill(inputs).map(sent).unwrap_or_default()
        });
        batches.collect()
    }

    /// Asserts every port in `ports` sends in each two batches in a row, and
    /// each port's counts go up by one.
    fn assert_fair(batches: &[Vec<(u8, u16)>], ports: u8) {
        for pair in batches.windows(2) {
            for port in 0..ports {
                let sends = pair.iter().flatten().any(|(p, _)| *p == port);
                assert!(sends, "port {port} starved in {pair:?}");
            }
        }
        for port in 0..ports {
            let counts = batches.iter().flatten().filter(|(p, _)| *p == port);
            let counts: Vec<u16> = counts.map(|(_, count)| *count).collect();
            assert!(counts.iter().zip(0..).all(|(c, i)| *c == i), "{counts:?}");
        }
    }

    /// Writes records to port `port`'s ring until it is full, counting on
    /// from `next`.
    fn top_up(rings: &[RingBuffer], port: u8, next: &mut u16, len: usize) {
        let ring = &rings[usize::from(port)];
        let mut writer = ring.writer(Notifier::default()).expect("a free writer");
        let mut record = vec![port; len];
        loop {
            record[1..3].copy_from_slice(&next.to_le_bytes());
            if writer.try_write(&record).is_err() {
                return;
            }
            *next += 1;
        }
    }

    #[test]
    fn test_ports_that_each_fill_half_take_turns() {
        // Each ring holds three 64-byte records, half a six-packet batch.
        let (rings, mut inputs) = sized_ports(&[64; 3], 3);
        let (mut batcher, _io) = batcher(&mut inputs, 6 * (PACKET_OVERHEAD + 64));
        let mut next = [0; 3];
        let batches = run(&mut inputs, &mut batcher, 8, |_| {
            for (port, next) in (0..).zip(&mut next) {
                top_up(&rings, port, next, 64);
            }
        });
        assert!(batches.iter().all(|batch| batch.len() == 6), "{batches:?}");
        assert_fair(&batches, 3);
    }

    #[test]
    fn test_one_port_rotates_to_itself() {
        let (rings, mut inputs) = sized_ports(&[16], 8);
        let (mut batcher, _io) = batcher(&mut inputs, 2 * (PACKET_OVERHEAD + 16));
        write(&rings, 0, 0..5, 16);
        let batches = run(&mut inputs, &mut batcher, 4, |_| {});
        assert_eq!(batcher.next, 0);
        let lengths: Vec<_> = batches.iter().map(Vec::len).collect();
        assert_eq!(lengths, [2, 2, 1, 0]);
        assert_fair(&batches[..3], 1);
    }

    #[test]
    fn test_a_flooding_port_does_not_starve_a_large_one() {
        const LARGE: usize = 1024;
        let (rings, mut inputs) = sized_ports(&[16, LARGE], 512);
        let (mut batcher, _io) = batcher(&mut inputs, LARGE + PACKET_OVERHEAD);
        let batches = run(&mut inputs, &mut batcher, 8, |cycle| {
            write(&rings, 0, 50 * cycle..50 * cycle + 50, 16);
            write(&rings, 1, cycle..cycle + 1, LARGE);
        });
        assert_fair(&batches, 2);
    }
}
