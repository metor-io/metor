//! Typed ports over ring handles.

use core::borrow::Borrow;
use core::marker::PhantomData;
use core::ops::Deref;

use crate::record::{DecodeError, EncodeError, Record};
use crate::system::PortDef;
use metor_fsw_3_ring::{NoWake, ReadError, View, WriteError, Writer, frame_len};
use metor_proto::types::Timestamp;

/// Returns the power-of-two capacity for a ring of `depth` records of `max_len` bytes.
pub fn ring_capacity(max_len: usize, depth: usize) -> Option<usize> {
    u32::try_from(max_len).ok()?;
    max_len.checked_add(2 * metor_fsw_3_ring::PAYLOAD_ALIGNMENT - 1)?;
    frame_len(max_len)
        .checked_mul(depth.max(2))?
        .checked_next_power_of_two()
}

/// An `UnsupportedAlignment` is a record aligned above what ring payloads provide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("frame alignment {alignment} exceeds ring payload alignment {supported}")]
pub struct UnsupportedAlignment {
    pub alignment: usize,
    pub supported: usize,
}

fn check_alignment<T: Record>() -> Result<(), UnsupportedAlignment> {
    let alignment = T::ALIGN;
    let supported = metor_fsw_3_ring::PAYLOAD_ALIGNMENT;
    if alignment > supported {
        return Err(UnsupportedAlignment {
            alignment,
            supported,
        });
    }
    Ok(())
}

/// A `SendError` is why a write published no record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("record of {len} bytes exceeds the port's {max}")]
    Oversize { len: usize, max: usize },
    #[error(transparent)]
    Encode(EncodeError),
    #[error(transparent)]
    Ring(WriteError),
}

/// A `RecvError` is why a drained record produced no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecvError {
    #[error(transparent)]
    Ring(ReadError),
    #[error(transparent)]
    Decode(DecodeError),
}

/// Rejects a record longer than the port's bound before it reaches the ring.
fn bounded(bytes: &[u8], max: usize) -> Result<&[u8], SendError> {
    if bytes.len() > max {
        return Err(SendError::Oversize {
            len: bytes.len(),
            max,
        });
    }
    Ok(bytes)
}

/// An `Output` is the writing half of one ring, publishing records of type `T`.
pub struct Output<T> {
    writer: Writer<NoWake>,
    scratch: Vec<u8>,
    _t: PhantomData<T>,
}

impl<T: Record> Output<T> {
    /// Binds a writer, rejecting records aligned above the ring guarantee.
    pub fn try_new(writer: Writer<NoWake>) -> Result<Self, UnsupportedAlignment> {
        check_alignment::<T>()?;
        Ok(Self {
            writer,
            scratch: vec![0; T::MAX_LEN],
            _t: PhantomData,
        })
    }

    /// Returns this port's entry in a bundle's `defs()` walk.
    pub fn def(name: &'static str) -> PortDef {
        PortDef {
            name: name.into(),
            record: T::NAME.into(),
            id: T::ID,
            max_len: T::MAX_LEN,
            alignment: T::ALIGN,
            depth: T::DEPTH,
        }
    }

    /// Publishes one value as one record.
    pub fn write(&mut self, value: &T) -> Result<(), SendError> {
        let bytes = value.encode(&mut self.scratch).map_err(SendError::Encode)?;
        let bytes = bounded(bytes, T::MAX_LEN)?;
        self.writer.try_write(bytes).map_err(SendError::Ring)
    }
}

/// An `Input` is the reading half of every ring feeding one port of type `T`.
pub struct Input<T> {
    views: Vec<View<NoWake>>,
    _t: PhantomData<T>,
}

impl<T: Record> Input<T> {
    /// Binds one view per producer, rejecting records aligned above the ring guarantee.
    pub fn try_new(views: Vec<View<NoWake>>) -> Result<Self, UnsupportedAlignment> {
        check_alignment::<T>()?;
        Ok(Self {
            views,
            _t: PhantomData,
        })
    }

    /// Returns this port's entry in a bundle's `defs()` walk.
    pub fn def(name: &'static str) -> PortDef {
        Output::<T>::def(name)
    }

    /// Reads every unread record, producer by producer in bind order.
    pub fn drain(&mut self) -> impl Iterator<Item = Result<T::Read<'_>, RecvError>> + '_ {
        self.views.iter_mut().flat_map(|view| {
            view.drain().map(|record| {
                let bytes = record.map_err(RecvError::Ring)?;
                T::decode(bytes).map_err(RecvError::Decode)
            })
        })
    }
}

impl<T: Record> Input<T> {
    /// Returns the newest record across producers
    ///
    /// Records without timestamps are produced in order of the producers
    pub fn latest(&mut self) -> Result<Option<Latest<'_, T>>, RecvError> {
        let mut best: Option<(Option<Timestamp>, T::Read<'_>)> = None;
        for view in self.views.iter_mut() {
            let Some(bytes) = view.try_latest_bytes().map_err(RecvError::Ring)? else {
                continue;
            };
            let value = T::decode(bytes).map_err(RecvError::Decode)?;
            let stamp = value.borrow().timestamp();
            if best.as_ref().is_none_or(|(b, _)| stamp > *b) {
                best = Some((stamp, value));
            }
        }
        Ok(best.map(|(_, decoded)| Latest { decoded }))
    }
}

/// A `Latest` holds a decoded record, borrowed from the input or owned.
pub struct Latest<'a, T: Record + 'a> {
    decoded: T::Read<'a>,
}

impl<'a, T: Record> Latest<'a, T> {
    /// Borrows the decoded record without decoding again.
    pub fn read(&self) -> &T {
        self.decoded.borrow()
    }

    /// Returns the decoded record: a borrow for a frame, an owned value for a message.
    pub fn into_inner(self) -> T::Read<'a> {
        self.decoded
    }
}

impl<T: Record> Deref for Latest<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.read()
    }
}

#[cfg(kani)]
mod proofs {
    use super::{SendError, bounded, ring_capacity};

    /// A bounded slice is never longer than the bound.
    #[kani::proof]
    fn bounded_never_exceeds_max() {
        let len: usize = kani::any();
        let max: usize = kani::any();
        kani::assume(len <= 64);
        let bytes = [0u8; 64];
        match bounded(&bytes[..len], max) {
            Ok(slice) => assert!(slice.len() <= max),
            Err(SendError::Oversize { len: l, max: m }) => assert!(l > m),
            Err(_) => unreachable!(),
        }
    }

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
    use crate::record::{DecodeError, EncodeError};
    use crate::tests::utils::{Fixed, Imu, Note, Stamped};
    use crate::{Frame, Record};

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
            capacity: ring_capacity(Imu::MAX_LEN, depth).expect("valid capacity"),
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
    fn a_port_def_names_its_record() {
        assert_eq!(Output::<Imu>::def("imu").record, Imu::NAME);
        assert_eq!(Input::<Note>::def("note").record, Note::NAME);
        assert_eq!(Input::<Imu>::def("imu"), Output::<Imu>::def("imu"));
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
        // Two 32-byte records fill the ring.
        out.write(&sample(1, 1.0)).expect("ring has room");
        assert_eq!(
            out.write(&sample(2, 2.0)),
            Err(SendError::Ring(WriteError::WouldBlock))
        );
    }

    #[test]
    fn short_record_is_corrupt() {
        let ring = ring(4);
        let mut writer = ring.writer(NoWake).expect("free writer");
        let mut input = Input::<Imu>::try_new(vec![ring.view(NoWake).expect("free slot")])
            .expect("supported alignment");
        writer.try_write(&[0u8; 4]).expect("ring has room");
        assert_eq!(
            input.latest().err(),
            Some(RecvError::Decode(DecodeError::Truncated))
        );
        assert_eq!(
            input.drain().next().unwrap().err(),
            Some(RecvError::Decode(DecodeError::Truncated))
        );
    }

    fn stamped_pair() -> (
        RingBuffer,
        RingBuffer,
        Output<Stamped>,
        Output<Stamped>,
        Input<Stamped>,
    ) {
        let (left, right) = (
            RingBuffer::create_in_memory(Config {
                capacity: ring_capacity(Stamped::MAX_LEN, 4).expect("valid"),
                max_readers: 1,
            }),
            RingBuffer::create_in_memory(Config {
                capacity: ring_capacity(Stamped::MAX_LEN, 4).expect("valid"),
                max_readers: 1,
            }),
        );
        let a = Output::try_new(left.writer(NoWake).expect("writer")).expect("aligned");
        let b = Output::try_new(right.writer(NoWake).expect("writer")).expect("aligned");
        let input = Input::try_new(vec![
            left.view(NoWake).expect("slot"),
            right.view(NoWake).expect("slot"),
        ])
        .expect("aligned");
        (left, right, a, b, input)
    }

    thread_local! {
        static DECODES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }

    struct Counted(Stamped);

    impl Record for Counted {
        const NAME: &'static str = "counted";
        const MAX_LEN: usize = Stamped::MAX_LEN;
        type Read<'a> = Self;

        fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
            self.0.encode(buf)
        }

        fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
            DECODES.set(DECODES.get() + 1);
            Stamped::decode(bytes).map(Self)
        }

        fn timestamp(&self) -> Option<Timestamp> {
            self.0.timestamp()
        }
    }

    #[test]
    fn latest_decodes_each_candidate_once_and_retains_the_winner() {
        let (_l, _r, mut a, mut b, mut input) = message_pair_two::<Counted>();
        DECODES.set(0);
        assert!(input.latest().expect("empty input").is_none());
        assert_eq!(DECODES.get(), 0);
        for (out, at) in [(&mut a, 1), (&mut b, 2)] {
            out.write(&Counted(Stamped {
                at: Timestamp(at),
                text: "sample".into(),
            }))
            .expect("fits");
        }
        let latest = input.latest().expect("valid").expect("record");
        assert_eq!(DECODES.get(), 2);
        assert_eq!(latest.read().0.at, Timestamp(2));
        assert!(core::ptr::eq(latest.read(), latest.read()));
        let owned: Counted = latest.into_inner();
        assert_eq!(owned.0.text, "sample");
        assert_eq!(DECODES.get(), 2);
    }

    #[test]
    fn latest_into_inner_keeps_frames_borrowed() {
        let (_ring, mut out, mut input) = pair(4);
        out.write(&sample(1, 2.0)).expect("fits");
        let frame: &Imu = input.latest().expect("valid").expect("record").into_inner();
        assert_eq!(frame.sample, 2.0);
        out.write(&sample(2, 3.0)).expect("fits");
        assert_eq!(frame.sample, 2.0);
    }

    #[test]
    fn latest_on_stamped_messages_orders_across_producers() {
        let (_l, _r, mut a, mut b, mut input) = stamped_pair();
        a.write(&Stamped {
            at: Timestamp(9),
            text: "a".into(),
        })
        .expect("fits");
        b.write(&Stamped {
            at: Timestamp(3),
            text: "b".into(),
        })
        .expect("fits");
        assert_eq!(input.latest().unwrap().unwrap().read().text, "a");
        b.write(&Stamped {
            at: Timestamp(11),
            text: "b2".into(),
        })
        .expect("fits");
        let newest = input.latest().unwrap().unwrap();
        assert_eq!(newest.read().text, "b2");
        assert_eq!(newest.read().at, Timestamp(11));
    }

    #[test]
    fn latest_on_unstamped_messages_keeps_the_earlier_producer() {
        let (_l, _r, mut a, mut b, mut input) = message_pair_two::<Fixed>();
        b.write(&Fixed { a: 2, b: 0.0 }).expect("fits");
        a.write(&Fixed { a: 1, b: 0.0 }).expect("fits");
        assert_eq!(input.latest().unwrap().unwrap().read().a, 1);
        assert_eq!(Fixed { a: 1, b: 0.0 }.timestamp(), None);
    }

    fn message_pair_two<T: Record>() -> (RingBuffer, RingBuffer, Output<T>, Output<T>, Input<T>) {
        let ring = || {
            RingBuffer::create_in_memory(Config {
                capacity: ring_capacity(T::MAX_LEN, 4).expect("valid"),
                max_readers: 1,
            })
        };
        let (left, right) = (ring(), ring());
        let a = Output::try_new(left.writer(NoWake).expect("writer")).expect("aligned");
        let b = Output::try_new(right.writer(NoWake).expect("writer")).expect("aligned");
        let input = Input::try_new(vec![
            left.view(NoWake).expect("slot"),
            right.view(NoWake).expect("slot"),
        ])
        .expect("aligned");
        (left, right, a, b, input)
    }

    fn message_pair<T: Record>() -> (RingBuffer, Output<T>, Input<T>) {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(T::MAX_LEN, 4).expect("valid capacity"),
            max_readers: 1,
        });
        let out = Output::try_new(ring.writer(NoWake).expect("free writer"))
            .expect("supported alignment");
        let input = Input::try_new(vec![ring.view(NoWake).expect("free slot")])
            .expect("supported alignment");
        (ring, out, input)
    }

    #[test]
    fn message_round_trips_through_write_and_drain() {
        let (_ring, mut out, mut input) = message_pair::<Note>();
        out.write(&Note { text: "hi".into() }).expect("fits");
        out.write(&Note {
            text: "there".into(),
        })
        .expect("fits");
        let seen: Vec<Note> = input.drain().map(|r| r.expect("decodes")).collect();
        assert_eq!(seen[0].text, "hi");
        assert_eq!(seen[1].text, "there");
        assert_eq!(input.drain().count(), 0);
    }

    #[test]
    fn oversize_message_leaves_the_ring_untouched() {
        let (_ring, mut out, mut input) = message_pair::<Note>();
        let long = Note {
            text: "x".repeat(100),
        };
        assert_eq!(
            out.write(&long),
            Err(SendError::Encode(EncodeError::Oversize {
                len: 101,
                max: 64
            }))
        );
        assert_eq!(input.drain().count(), 0);
    }

    #[test]
    fn a_corrupt_record_does_not_stop_the_drain() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(Fixed::MAX_LEN, 4).expect("valid capacity"),
            max_readers: 1,
        });
        let mut writer = ring.writer(NoWake).expect("free writer");
        let mut input = Input::<Fixed>::try_new(vec![ring.view(NoWake).expect("free slot")])
            .expect("supported alignment");
        let mut buf = [0u8; 16];
        let good = Fixed { a: 1, b: 2.0 };
        writer.try_write(&[0xff; 3]).expect("ring has room");
        writer
            .try_write(good.encode(&mut buf).expect("fits"))
            .expect("ring has room");
        let seen: Vec<_> = input.drain().collect();
        assert_eq!(
            seen,
            vec![Err(RecvError::Decode(DecodeError::Codec)), Ok(good)]
        );
    }

    #[test]
    fn frames_drain_by_reference_and_messages_by_value() {
        let (_ring, mut out, mut input) = pair(4);
        out.write(&sample(1, 1.0)).expect("ring has room");
        let borrowed: Option<&Imu> = input.drain().next().map(|r| r.expect("decodes"));
        assert_eq!(borrowed.map(|f| f.sample), Some(1.0));

        let (_ring, mut out, mut input) = message_pair::<Fixed>();
        out.write(&Fixed { a: 3, b: 0.5 }).expect("fits");
        let owned: Option<Fixed> = input.drain().next().map(|r| r.expect("decodes"));
        assert_eq!(owned, Some(Fixed { a: 3, b: 0.5 }));
    }
}
