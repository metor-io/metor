//! Typed ports over ring handles.

use core::marker::PhantomData;
use core::mem::align_of;
use core::ops::Deref;

use crate::frame::Frame;
use crate::system::PortDef;
use metor_fsw_3_ring::{NoWake, ReadError, ReadGrant, View, WriteError, Writer, frame_len};
use metor_proto::types::Timestamp;

/// Returns the capacity needed for a ring with `depth` records of `max_size` bytes each.
///
/// This is always rounded to a power of two.
pub fn ring_capacity(max_size: usize, depth: usize) -> Option<usize> {
    u32::try_from(max_size).ok()?;
    max_size.checked_add(2 * metor_fsw_3_ring::PAYLOAD_ALIGNMENT - 1)?;
    frame_len(max_size)
        .checked_mul(depth.max(2))?
        .checked_next_power_of_two()
}

/// A frame requires more alignment than ring payloads provide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("frame alignment {alignment} exceeds ring payload alignment {supported}")]
pub struct UnsupportedAlignment {
    pub alignment: usize,
    pub supported: usize,
}

fn check_alignment<F>() -> Result<(), UnsupportedAlignment> {
    let alignment = align_of::<F>();
    let supported = metor_fsw_3_ring::PAYLOAD_ALIGNMENT;
    if alignment > supported {
        return Err(UnsupportedAlignment {
            alignment,
            supported,
        });
    }
    Ok(())
}

/// The writing half of one ring, publishing frames of type `F`.
pub struct Output<F> {
    writer: Writer<NoWake>,
    _f: PhantomData<F>,
}

impl<F: Frame> Output<F> {
    /// Bind a writer, rejecting frames whose alignment exceeds the ring guarantee.
    pub fn try_new(writer: Writer<NoWake>) -> Result<Self, UnsupportedAlignment> {
        check_alignment::<F>()?;
        Ok(Self {
            writer,
            _f: PhantomData,
        })
    }

    /// This port's entry in a bundle's `defs()` walk.
    pub fn def(name: &'static str) -> PortDef {
        PortDef {
            name,
            frame: F::ID,
            max_size: F::MAX_SIZE,
            alignment: align_of::<F>(),
        }
    }

    /// Publish one frame as one record.
    pub fn write(&mut self, frame: &F) -> Result<(), WriteError> {
        self.writer.try_write(frame.as_bytes())
    }
}

/// Reader for a specific frame
pub struct Input<F> {
    views: Vec<View<NoWake>>,
    _f: PhantomData<F>,
}

impl<F: Frame> Input<F> {
    /// Creates a new input with the given views, errors when a frame has an unsupported alignment.
    pub fn try_new(views: Vec<View<NoWake>>) -> Result<Self, UnsupportedAlignment> {
        check_alignment::<F>()?;
        Ok(Self {
            views,
            _f: PhantomData,
        })
    }

    /// Returns a [`PortDef`] for this frame type
    pub fn def(name: &'static str) -> PortDef {
        PortDef {
            name,
            frame: F::ID,
            max_size: F::MAX_SIZE,
            alignment: align_of::<F>(),
        }
    }

    /// Returns the latest frame in the input.
    ///
    /// Internally this loops through all of the views, and finds the one with the latest timestamp.
    pub fn latest(&mut self) -> Result<Option<FrameGrant<'_, F>>, ReadError> {
        let mut latest: Option<(Timestamp, ReadGrant<'_>)> = None;
        for view in self.views.iter_mut() {
            let Some(grant) = view.try_latest()? else {
                continue;
            };
            let stamp = frame_of::<F>(&grant)?.timestamp();
            if latest.as_ref().is_none_or(|(b, _)| stamp > *b) {
                latest = Some((stamp, grant));
            }
        }
        latest.map(|(_, grant)| FrameGrant::new(grant)).transpose()
    }

    /// Iterator that drains all frames from the input.
    ///
    /// This iterator runs in the order of the internal views, so frames from
    /// different producers are not ordered by timestamp.
    pub fn drain(&mut self) -> impl Iterator<Item = Result<&F, ReadError>> + '_ {
        self.views
            .iter_mut()
            .flat_map(|view| view.drain().map(|record| frame_of::<F>(record?)))
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
    use super::ring_capacity;

    /// A capacity always holds two records of the largest size the ring's
    /// 32-bit length field can address.
    #[kani::proof]
    fn capacity_fits_two_records() {
        let max_size: u32 = kani::any();
        let depth: usize = kani::any();
        let max_size = max_size as usize;
        if let Some(capacity) = ring_capacity(max_size, depth) {
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
    use crate::tests::utils::Imu;
    use crate::{Componentize, Frame};

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
    #[repr(C, align(16))]
    struct Aligned {
        #[frame(timestamp)]
        timestamp: Timestamp,
        value: u64,
    }

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
    #[repr(C, align(32))]
    struct Overaligned {
        #[frame(timestamp)]
        timestamp: Timestamp,
        value: [u64; 3],
    }

    #[test]
    fn aligned_frames_remain_readable_through_wraps() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: 128,
            max_readers: 1,
        });
        let mut output = Output::<Aligned>::try_new(ring.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let mut input = Input::<Aligned>::try_new(vec![ring.view(NoWake).expect("free slot")])
            .expect("supported alignment");
        for value in 0..20 {
            output
                .write(&Aligned {
                    timestamp: Timestamp(value as i64),
                    value,
                })
                .expect("ring has room");
            let frame = input.latest().expect("aligned record").expect("record");
            assert_eq!(frame.value, value);
        }
    }

    #[test]
    fn unsupported_alignment_is_rejected_at_construction() {
        let ring = ring(4);
        let error = UnsupportedAlignment {
            alignment: 32,
            supported: 16,
        };
        assert_eq!(
            Output::<Overaligned>::try_new(ring.writer(NoWake).expect("free writer")).err(),
            Some(error)
        );
        assert_eq!(
            Input::<Overaligned>::try_new(vec![ring.view(NoWake).expect("free slot")]).err(),
            Some(error)
        );
        assert_eq!(Input::<Overaligned>::try_new(Vec::new()).err(), Some(error));
        assert!(ring.writer(NoWake).is_ok());
        assert_eq!(Input::<Overaligned>::def("input").alignment, 32);
        assert_eq!(Output::<Aligned>::def("output").alignment, 16);
    }

    fn sample(ts: i64, sample: f64) -> Imu {
        Imu::new(ts, sample)
    }

    fn ring(depth: usize) -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(Imu::MAX_SIZE, depth).expect("valid capacity"),
            max_readers: 4,
        })
    }

    fn pair(depth: usize) -> (RingBuffer, Output<Imu>, Input<Imu>) {
        let ring = ring(depth);
        let out = Output::try_new(ring.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let input = Input::try_new(vec![ring.view(NoWake).expect("free slot")])
            .expect("supported alignment");
        (ring, out, input)
    }

    #[test]
    fn capacity_is_power_of_two_holding_two_records() {
        let capacity = ring_capacity(64, 1).expect("valid capacity");
        assert!(capacity.is_power_of_two());
        assert!(capacity >= 2 * frame_len(64));
        assert!(ring_capacity(usize::MAX, 4).is_none());
        if let Ok(size) = usize::try_from(1u64 << 40) {
            assert!(ring_capacity(size, 4).is_none());
        }
        #[cfg(target_pointer_width = "32")]
        assert!(ring_capacity(u32::MAX as usize, 4).is_none());
        assert!(ring_capacity(1 << 30, usize::MAX).is_none());
    }

    #[test]
    fn write_then_latest() {
        let (_ring, mut out, mut input) = pair(4);
        out.write(&sample(7, 42.0)).expect("ring has room");
        let got = input.latest().expect("valid record").expect("one record");
        assert_eq!(got.sample, 42.0);
        assert_eq!(got.timestamp, Timestamp(7));
    }

    #[test]
    fn latest_repeats_the_pinned_record() {
        let (_ring, mut out, mut input) = pair(4);
        out.write(&sample(1, 1.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);
    }

    #[test]
    fn drain_visits_records_in_order() {
        let (_ring, mut out, mut input) = pair(8);
        for i in 0..3 {
            out.write(&sample(i, i as f64)).expect("ring has room");
        }
        let seen = input
            .drain()
            .flat_map(|r| r.ok())
            .map(|r| r.sample)
            .collect::<Vec<_>>();
        assert_eq!(seen, vec![0.0, 1.0, 2.0]);
        let seen = input.drain().collect::<Vec<_>>();
        assert!(seen.is_empty());
    }

    #[test]
    fn latest_picks_the_greater_timestamp_across_producers() {
        let (left, right) = (ring(4), ring(4));
        let mut a = Output::<Imu>::try_new(left.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let mut b = Output::<Imu>::try_new(right.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let mut input = Input::<Imu>::try_new(vec![
            left.view(NoWake).expect("free slot"),
            right.view(NoWake).expect("free slot"),
        ])
        .expect("supported alignment");
        a.write(&sample(9, 1.0)).expect("ring has room");
        b.write(&sample(3, 2.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);

        b.write(&sample(11, 3.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 3.0);
    }

    #[test]
    fn latest_ties_go_to_the_earlier_producer() {
        let (left, right) = (ring(4), ring(4));
        let mut a = Output::<Imu>::try_new(left.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let mut b = Output::<Imu>::try_new(right.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let mut input = Input::<Imu>::try_new(vec![
            left.view(NoWake).expect("free slot"),
            right.view(NoWake).expect("free slot"),
        ])
        .expect("supported alignment");
        a.write(&sample(5, 1.0)).expect("ring has room");
        b.write(&sample(5, 2.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);
    }

    #[test]
    fn drain_visits_producers_in_edge_order() {
        let (left, right) = (ring(8), ring(8));
        let mut a = Output::<Imu>::try_new(left.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let mut b = Output::<Imu>::try_new(right.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let mut input = Input::<Imu>::try_new(vec![
            left.view(NoWake).expect("free slot"),
            right.view(NoWake).expect("free slot"),
        ])
        .expect("supported alignment");
        b.write(&sample(0, 10.0)).expect("ring has room");
        a.write(&sample(1, 1.0)).expect("ring has room");
        a.write(&sample(2, 2.0)).expect("ring has room");
        let seen = input
            .drain()
            .flat_map(|r| r.ok())
            .map(|r| r.sample)
            .collect::<Vec<_>>();
        assert_eq!(seen, vec![1.0, 2.0, 10.0]);
    }

    #[test]
    fn unconnected_input_reads_nothing() {
        let mut input = Input::<Imu>::try_new(Vec::new()).expect("supported alignment");
        assert!(input.latest().expect("valid").is_none());
        assert_eq!(input.drain().count(), 0);
    }

    #[test]
    fn full_ring_would_block() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: 64,
            max_readers: 1,
        });
        let mut out = Output::try_new(ring.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let mut input = Input::<Imu>::try_new(vec![ring.view(NoWake).expect("free slot")])
            .expect("supported alignment");
        out.write(&sample(0, 0.0)).expect("ring has room");
        let pinned = input.latest().expect("valid").expect("record");
        assert_eq!(pinned.sample, 0.0);
        drop(pinned);
        // Two 32-byte records fill the ring.
        out.write(&sample(1, 1.0)).expect("ring has room");
        assert_eq!(out.write(&sample(2, 2.0)), Err(WriteError::WouldBlock));
    }

    #[test]
    fn short_record_is_corrupt() {
        let ring = ring(4);
        let mut writer = ring.writer(NoWake).expect("free writer");
        let mut input = Input::<Imu>::try_new(vec![ring.view(NoWake).expect("free slot")])
            .expect("supported alignment");
        writer.try_write(&[0u8; 4]).expect("ring has room");
        assert_eq!(input.latest().err(), Some(ReadError::Corrupt));
        assert_eq!(
            input.drain().next().unwrap().err(),
            Some(ReadError::Corrupt)
        );
    }
}
