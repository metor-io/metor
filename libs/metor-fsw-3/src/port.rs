//! Typed ports over ring handles.
//!
//! [`Output<F>`] owns the single writer of one ring; [`Input<F>`] holds one
//! view per producer, so fan-in is a list of views and an unconnected input is
//! an empty list. A frame's `#[repr(C)]` bytes are the record, so a write is
//! one `try_write` and a read is a borrow of the record in place.

use core::marker::PhantomData;
use core::ops::Deref;

use crate::frame::Frame;
use crate::system::PortDef;
use metor_fsw_3_ring::{NoWake, ReadError, ReadGrant, View, WriteError, Writer, frame_len};
use metor_proto::types::Timestamp;

/// Power-of-two ring capacity holding `depth.max(2)` records of at most
/// `max_size` payload bytes, or `None` if the size or capacity cannot be
/// represented. The ring's record length is 32 bits.
///
/// The floor of two records is what lets a latest-wins reader pin one record
/// while the writer fills another.
pub fn capacity_for(max_size: usize, depth: usize) -> Option<usize> {
    u32::try_from(max_size).ok()?;
    frame_len(max_size)
        .checked_mul(depth.max(2))?
        .checked_next_power_of_two()
}

/// The writing half of one ring, publishing frames of type `F`.
pub struct Output<F> {
    writer: Writer<NoWake>,
    _f: PhantomData<F>,
}

impl<F: Frame> Output<F> {
    pub fn new(writer: Writer<NoWake>) -> Self {
        Self {
            writer,
            _f: PhantomData,
        }
    }

    /// This port's entry in a bundle's `defs()` walk.
    pub fn def(name: &'static str) -> PortDef {
        PortDef {
            name,
            frame: F::ID,
            max_size: F::MAX_SIZE,
        }
    }

    /// Publish one frame as one record.
    pub fn write(&mut self, frame: &F) -> Result<(), WriteError> {
        self.writer.try_write(frame.as_bytes())
    }
}

/// The reading half of a port: one view per producer, in edge order.
pub struct Input<F> {
    views: Vec<View<NoWake>>,
    _f: PhantomData<F>,
}

impl<F: Frame> Input<F> {
    pub fn new(views: Vec<View<NoWake>>) -> Self {
        Self {
            views,
            _f: PhantomData,
        }
    }

    /// This port's entry in a bundle's `defs()` walk.
    pub fn def(name: &'static str) -> PortDef {
        PortDef {
            name,
            frame: F::ID,
            max_size: F::MAX_SIZE,
        }
    }

    /// The newest record across every producer, by frame timestamp. Ties go
    /// to the earlier producer.
    ///
    /// Each view is advanced to its own newest record, which stays pinned, so
    /// a cycle with no new data sees the same record again.
    pub fn latest(&mut self) -> Result<Option<FrameGrant<'_, F>>, ReadError> {
        let mut best: Option<(Timestamp, ReadGrant<'_>)> = None;
        for view in self.views.iter_mut() {
            let Some(grant) = view.try_latest()? else {
                continue;
            };
            let stamp = frame_of::<F>(&grant)?.timestamp();
            if best.as_ref().is_none_or(|(b, _)| stamp > *b) {
                best = Some((stamp, grant));
            }
        }
        best.map(|(_, grant)| FrameGrant::new(grant)).transpose()
    }

    /// Hand `f` every committed record, producer by producer in edge order.
    pub fn drain(&mut self, mut f: impl FnMut(&F)) -> Result<(), ReadError> {
        for view in &mut self.views {
            while let Some(grant) = view.try_read()? {
                f(frame_of::<F>(&grant)?);
            }
        }
        Ok(())
    }
}

/// One record, borrowed in place and read as the frame itself.
pub struct FrameGrant<'a, F> {
    grant: ReadGrant<'a>,
    _f: PhantomData<F>,
}

impl<'a, F: Frame> FrameGrant<'a, F> {
    fn new(grant: ReadGrant<'a>) -> Result<Self, ReadError> {
        frame_of::<F>(&grant)?;
        Ok(Self {
            grant,
            _f: PhantomData,
        })
    }
}

impl<F: Frame> Deref for FrameGrant<'_, F> {
    type Target = F;
    fn deref(&self) -> &F {
        // PANIC Safety: the record was checked against `F` when the grant was
        // constructed, and a grant pins its record.
        frame_of::<F>(&self.grant).expect("record checked at construction")
    }
}

/// Read the fixed region of a record. A record shorter than `F` is a
/// corrupt region, not a panic.
fn frame_of<F: Frame>(record: &[u8]) -> Result<&F, ReadError> {
    F::ref_from_prefix(record)
        .map(|(frame, _)| frame)
        .map_err(|_| ReadError::Corrupt)
}

#[cfg(kani)]
mod proofs {
    use super::capacity_for;

    /// A capacity always holds two records of the largest size the ring's
    /// 32-bit length field can address.
    #[kani::proof]
    fn capacity_fits_two_records() {
        let max_size: u32 = kani::any();
        let depth: usize = kani::any();
        let max_size = max_size as usize;
        if let Some(capacity) = capacity_for(max_size, depth) {
            assert!(capacity.is_power_of_two());
            assert!(capacity >= 2 * metor_fsw_3_ring::frame_len(max_size));
        }
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer, WriteError};
    use metor_proto::types::Timestamp;
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    use super::*;
    use crate::{Componentize, Frame};

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
    #[frame(name = "sample")]
    #[repr(C)]
    struct Sample {
        #[frame(timestamp)]
        timestamp: Timestamp,
        value: u64,
    }

    fn sample(ts: i64, value: u64) -> Sample {
        Sample {
            timestamp: Timestamp(ts),
            value,
        }
    }

    fn ring(depth: usize) -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: capacity_for(Sample::MAX_SIZE, depth).expect("valid capacity"),
            max_readers: 4,
        })
    }

    fn pair(depth: usize) -> (RingBuffer, Output<Sample>, Input<Sample>) {
        let ring = ring(depth);
        let out = Output::new(ring.writer(NoWake).expect("free writer"));
        let input = Input::new(vec![ring.view(NoWake).expect("free slot")]);
        (ring, out, input)
    }

    #[test]
    fn capacity_is_power_of_two_holding_two_records() {
        let capacity = capacity_for(64, 1).expect("valid capacity");
        assert!(capacity.is_power_of_two());
        assert!(capacity >= 2 * frame_len(64));
        assert!(capacity_for(usize::MAX, 4).is_none());
        assert!(capacity_for(1 << 40, 4).is_none());
        assert!(capacity_for(1 << 30, usize::MAX).is_none());
    }

    #[test]
    fn write_then_latest() {
        let (_ring, mut out, mut input) = pair(4);
        out.write(&sample(7, 42)).expect("ring has room");
        let got = input.latest().expect("valid record").expect("one record");
        assert_eq!(got.value, 42);
        assert_eq!(got.timestamp, Timestamp(7));
    }

    #[test]
    fn latest_repeats_the_pinned_record() {
        let (_ring, mut out, mut input) = pair(4);
        out.write(&sample(1, 1)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").value, 1);
        assert_eq!(input.latest().expect("valid").expect("record").value, 1);
    }

    #[test]
    fn drain_visits_records_in_order() {
        let (_ring, mut out, mut input) = pair(8);
        for i in 0..3 {
            out.write(&sample(i, i as u64)).expect("ring has room");
        }
        let mut seen = Vec::new();
        input.drain(|f| seen.push(f.value)).expect("valid records");
        assert_eq!(seen, vec![0, 1, 2]);
        seen.clear();
        input.drain(|f| seen.push(f.value)).expect("valid records");
        assert!(seen.is_empty());
    }

    #[test]
    fn latest_picks_the_greater_timestamp_across_producers() {
        let (left, right) = (ring(4), ring(4));
        let mut a = Output::<Sample>::new(left.writer(NoWake).expect("free writer"));
        let mut b = Output::<Sample>::new(right.writer(NoWake).expect("free writer"));
        let mut input = Input::<Sample>::new(vec![
            left.view(NoWake).expect("free slot"),
            right.view(NoWake).expect("free slot"),
        ]);
        a.write(&sample(9, 1)).expect("ring has room");
        b.write(&sample(3, 2)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").value, 1);

        b.write(&sample(11, 3)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").value, 3);
    }

    #[test]
    fn latest_ties_go_to_the_earlier_producer() {
        let (left, right) = (ring(4), ring(4));
        let mut a = Output::<Sample>::new(left.writer(NoWake).expect("free writer"));
        let mut b = Output::<Sample>::new(right.writer(NoWake).expect("free writer"));
        let mut input = Input::<Sample>::new(vec![
            left.view(NoWake).expect("free slot"),
            right.view(NoWake).expect("free slot"),
        ]);
        a.write(&sample(5, 1)).expect("ring has room");
        b.write(&sample(5, 2)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").value, 1);
    }

    #[test]
    fn drain_visits_producers_in_edge_order() {
        let (left, right) = (ring(8), ring(8));
        let mut a = Output::<Sample>::new(left.writer(NoWake).expect("free writer"));
        let mut b = Output::<Sample>::new(right.writer(NoWake).expect("free writer"));
        let mut input = Input::<Sample>::new(vec![
            left.view(NoWake).expect("free slot"),
            right.view(NoWake).expect("free slot"),
        ]);
        b.write(&sample(0, 10)).expect("ring has room");
        a.write(&sample(1, 1)).expect("ring has room");
        a.write(&sample(2, 2)).expect("ring has room");
        let mut seen = Vec::new();
        input.drain(|f| seen.push(f.value)).expect("valid records");
        assert_eq!(seen, vec![1, 2, 10]);
    }

    #[test]
    fn unconnected_input_reads_nothing() {
        let mut input = Input::<Sample>::new(Vec::new());
        assert!(input.latest().expect("valid").is_none());
        let mut seen = 0;
        input.drain(|_| seen += 1).expect("valid");
        assert_eq!(seen, 0);
    }

    #[test]
    fn full_ring_would_block() {
        let (_ring, mut out, mut input) = pair(2);
        out.write(&sample(0, 0)).expect("ring has room");
        let pinned = input.latest().expect("valid").expect("record");
        assert_eq!(pinned.value, 0);
        drop(pinned);
        // The pinned record plus one more fills a depth-2 ring.
        out.write(&sample(1, 1)).expect("ring has room");
        assert_eq!(out.write(&sample(2, 2)), Err(WriteError::WouldBlock));
    }

    #[test]
    fn short_record_is_corrupt() {
        let ring = ring(4);
        let mut writer = ring.writer(NoWake).expect("free writer");
        let mut input = Input::<Sample>::new(vec![ring.view(NoWake).expect("free slot")]);
        writer.try_write(&[0u8; 4]).expect("ring has room");
        assert_eq!(input.latest().err(), Some(ReadError::Corrupt));
        assert_eq!(input.drain(|_| ()).err(), Some(ReadError::Corrupt));
    }
}
