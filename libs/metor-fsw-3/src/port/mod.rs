//! Typed ports over ring handles.

use core::borrow::Borrow;
use core::cell::Cell;
use core::marker::PhantomData;
use core::ops::Deref;
use std::borrow::Cow;

use crate::coordinator::OutputSource;
use crate::def::{DefCx, DefError};
use crate::record::{Bytes, DecodeError, EncodeError, Record};
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};
use metor_fsw_3_ring::{
    NoWake, Notifier, PAYLOAD_ALIGNMENT, ReadError, View, WakeSink, WakeSource, WriteError, Writer,
    frame_len,
};
use metor_proto::types::Timestamp;

mod readiness;
mod sample;

use readiness::Waiters;
pub use sample::Sample;
pub(crate) use sample::{Sampler, Samplers};

/// Returns the power-of-two capacity for a ring of `depth` records of `max_len` bytes.
pub fn ring_capacity(max_len: usize, depth: usize) -> Option<usize> {
    u32::try_from(max_len).ok()?;
    max_len.checked_add(2 * PAYLOAD_ALIGNMENT - 1)?;
    frame_len(max_len)
        .checked_mul(depth.max(2))?
        .checked_next_power_of_two()
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
    #[error("port is already mid-write")]
    Busy,
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

/// A scratch buffer lent out of its cell for one write, returned on drop.
struct Lent<'a> {
    cell: &'a Cell<Option<Box<[u8]>>>,
    buf: Box<[u8]>,
}

impl<'a> Lent<'a> {
    fn take(cell: &'a Cell<Option<Box<[u8]>>>) -> Result<Self, SendError> {
        let buf = cell.take().ok_or(SendError::Busy)?;
        Ok(Self { cell, buf })
    }
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        self.cell.set(Some(core::mem::take(&mut self.buf)));
    }
}

/// An `Output` is the writing half of one ring, publishing records of type `T`.
///
/// `W` is the endpoint notified after each write. System outputs use [`NoWake`];
/// adapter writers feeding async inputs use [`Notifier`].
pub struct Output<T: ?Sized, W: WakeSource = NoWake> {
    writer: Writer<W>,
    /// Empty while a write holds it.
    scratch: Cell<Option<Box<[u8]>>>,
    /// The longest record this port takes: the type's bound, or the binding's
    /// for a dynamic port, whose record the type does not know.
    max_len: usize,
    _t: PhantomData<T>,
}

impl<T: Record + ?Sized, W: WakeSource> Output<T, W> {
    /// Binds a writer. A record aligned above the ring guarantee fails the build.
    pub fn new(writer: Writer<W>) -> Self {
        const {
            assert!(
                T::ALIGN <= PAYLOAD_ALIGNMENT,
                "record aligned above ring payloads"
            )
        };
        Self {
            writer,
            scratch: Cell::new(Some(vec![0; T::MAX_LEN].into_boxed_slice())),
            max_len: T::MAX_LEN,
            _t: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn try_new(writer: Writer<W>) -> Result<Self, core::convert::Infallible> {
        Ok(Self::new(writer))
    }

    /// This port's entry in a bundle's `defs()` walk, announcing its record
    /// under `cx`'s root for `name`.
    pub fn def(cx: &DefCx<'_>, name: &'static str) -> PortDef {
        PortDef {
            name: name.into(),
            record: T::NAME.into(),
            id: T::ID,
            max_len: T::MAX_LEN,
            alignment: T::ALIGN,
            depth: T::DEPTH,
            layout: T::schema().layout(),
            schema: T::schema_at(&cx.root(name)),
        }
    }

    /// Publishes one value as one record.
    /// An `encode` that writes to this same port gets [`SendError::Busy`].
    pub fn write(&self, value: &T) -> Result<(), SendError> {
        let mut scratch = Lent::take(&self.scratch)?;
        let bytes = value.encode(&mut scratch.buf).map_err(SendError::Encode)?;
        let bytes = bounded(bytes, self.max_len)?;
        self.writer.try_write(bytes).map_err(SendError::Ring)
    }
}

impl<W: WakeSource> Output<Bytes, W> {
    /// Binds a dynamic port's writer, bounding it by the record `max_len` its
    /// binding named.
    pub fn bytes(writer: Writer<W>, max_len: usize) -> Self {
        Self {
            writer,
            scratch: Cell::new(Some(Box::default())),
            max_len,
            _t: PhantomData,
        }
    }

    /// Publishes a record's bytes as they arrived, without encoding.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), SendError> {
        let bytes = bounded(bytes, self.max_len)?;
        self.writer.try_write(bytes).map_err(SendError::Ring)
    }
}

/// An `Input` is the reading half of every ring feeding one port of type `T`.
///
/// `W` is the endpoint each view waits on. Async inputs wait on every producer,
/// whether their [`Notifier`] handles are shared or independent.
pub struct Input<T: ?Sized, W: WakeSink = NoWake> {
    views: Vec<View<W>>,
    waiters: Waiters,
    _t: PhantomData<T>,
}

impl<T: Record + ?Sized, W: WakeSink> Input<T, W> {
    /// Binds one view per producer. A record aligned above the ring guarantee
    /// fails the build.
    pub fn new(views: Vec<View<W>>) -> Self {
        const {
            assert!(
                T::ALIGN <= PAYLOAD_ALIGNMENT,
                "record aligned above ring payloads"
            )
        };
        Self {
            waiters: Waiters::new(&views),
            views,
            _t: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn try_new(views: Vec<View<W>>) -> Result<Self, core::convert::Infallible> {
        Ok(Self::new(views))
    }

    /// This port's entry in a bundle's `defs()` walk.
    pub fn def(cx: &DefCx<'_>, name: &'static str) -> PortDef {
        Output::<T>::def(cx, name)
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

    /// Applies deferred drain consumption, then reports the first producer with a record.
    fn ready(&mut self) -> Option<usize> {
        self.views.iter_mut().position(|view| {
            view.settle();
            view.has_record()
        })
    }
}

impl<T: Record + ?Sized> Input<T, Notifier> {
    /// Waits for the next record, then reads it from the first producer holding one.
    ///
    /// Never resolves on a port with no producer.
    pub async fn next(&mut self) -> Result<T::Read<'_>, RecvError> {
        let at = readiness::wait(self).await;
        // PANIC Safety: `ready` located a record on this view and nothing
        // consumed it since; the view is this port's alone.
        let bytes = self.views[at]
            .drain()
            .next()
            .expect("a located record")
            .map_err(RecvError::Ring)?;
        T::decode(bytes).map_err(RecvError::Decode)
    }
}

/// One port per config edge no declared port took, under the config's name
/// and its producer's definition.
pub fn dyn_input_defs(
    cx: &DefCx<'_>,
    declared: &[Cow<'static, str>],
) -> Result<Vec<PortDef>, DefError> {
    let mut ports: Vec<PortDef> = Vec::new();
    for (name, producer) in cx.inputs {
        if declared.iter().any(|taken| taken == name) {
            continue;
        }
        if ports.iter().any(|port| port.name == *name) {
            return Err(DefError::FanIn {
                port: name.to_string(),
            });
        }
        ports.push(PortDef {
            name: name.to_string().into(),
            ..(*producer).clone()
        });
    }
    Ok(ports)
}

/// One port per config output no declared port took, defined by the record it
/// names or the definition `metor build` resolved for it.
pub fn dyn_output_defs(
    cx: &DefCx<'_>,
    declared: &[Cow<'static, str>],
) -> Result<Vec<PortDef>, DefError> {
    cx.outputs
        .iter()
        .filter(|output| !declared.iter().any(|taken| *taken == output.port))
        .map(|output| match &output.source {
            OutputSource::Record(record) => cx.records.port(&output.port, record),
            OutputSource::Def(def) => Ok(PortDef {
                name: output.port.clone().into(),
                ..def.clone()
            }),
            OutputSource::Peer(_) => Err(DefError::Unresolved {
                port: output.port.clone(),
            }),
        })
        .collect()
}

/// A `DynInputs` is the input side of a system whose ports the config lists.
///
/// Each port carries the definition it computed from the config and the bytes
/// its producer wrote, undecoded.
pub struct DynInputs<W: WakeSink = NoWake> {
    ports: Vec<(PortDef, Input<Bytes, W>)>,
}

impl<W: WakeSink> Default for DynInputs<W> {
    fn default() -> Self {
        Self { ports: Vec::new() }
    }
}

impl<W: WakeSink> DynInputs<W> {
    /// Every port's definition and the records waiting on it.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&PortDef, &mut Input<Bytes, W>)> {
        self.ports.iter_mut().map(|(def, input)| (&*def, input))
    }
}

impl DynInputs<Notifier> {
    /// Waits until any port has a record.
    ///
    /// Never resolves while no port has a producer.
    pub async fn any_ready(&mut self) {
        readiness::wait(self).await;
    }
}

impl<W: WakeSink> SystemInputs<W> for DynInputs<W> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        dyn_input_defs(cx, &[])
    }

    fn bind(inputs: Vec<InputBinding<W>>) -> Self {
        let ports = inputs
            .into_iter()
            .map(|b| (b.def, Input::new(b.views)))
            .collect();
        Self { ports }
    }
}

/// A `DynOutputs` is the output side of a system whose ports the config lists.
pub struct DynOutputs<W: WakeSource = NoWake> {
    ports: Vec<(PortDef, Output<Bytes, W>)>,
}

impl<W: WakeSource> Default for DynOutputs<W> {
    fn default() -> Self {
        Self { ports: Vec::new() }
    }
}

impl<W: WakeSource> DynOutputs<W> {
    /// Every port's definition and the writer publishing on it.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&PortDef, &mut Output<Bytes, W>)> {
        self.ports.iter_mut().map(|(def, output)| (&*def, output))
    }
}

impl<W: WakeSource> SystemOutputs<W> for DynOutputs<W> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        dyn_output_defs(cx, &[])
    }

    fn bind(outputs: Vec<OutputBinding<W>>) -> Self {
        let ports = outputs
            .into_iter()
            .map(|b| {
                let output = Output::bytes(b.writer, b.def.max_len);
                (b.def, output)
            })
            .collect();
        Self { ports }
    }
}

/// A `Latest` holds a decoded record, borrowed from the input or owned.
pub struct Latest<'a, T: Record + ?Sized + 'a> {
    decoded: T::Read<'a>,
}

impl<'a, T: Record + ?Sized> Latest<'a, T> {
    /// Borrows the decoded record without decoding again.
    pub fn read(&self) -> &T {
        self.decoded.borrow()
    }

    /// Returns the decoded record: a borrow for a frame, an owned value for a message.
    pub fn into_inner(self) -> T::Read<'a> {
        self.decoded
    }
}

impl<T: Record + ?Sized> Deref for Latest<'_, T> {
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
    fn test_bounded_slice_length() {
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
    fn test_capacity_fits_two_records() {
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

    #[test]
    fn test_aligned_frames_wrap() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: 128,
            max_readers: 1,
        });
        let output = Output::<Aligned>::new(ring.writer(NoWake).expect("free writer"));
        let mut input = Input::<Aligned>::new(vec![ring.view(NoWake).expect("free slot")]);
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
        let out = Output::new(ring.writer(NoWake).expect("free writer"));
        let input = Input::new(vec![ring.view(NoWake).expect("free slot")]);
        (ring, out, input)
    }

    #[test]
    fn test_ring_capacity() {
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
    fn test_write_then_latest() {
        let (_ring, out, mut input) = pair(4);
        out.write(&sample(7, 42.0)).expect("ring has room");
        let got = input.latest().expect("valid record").expect("one record");
        assert_eq!(got.sample, 42.0);
        assert_eq!(got.timestamp, Timestamp(7));
    }

    #[test]
    fn test_latest_repeats_pinned_record() {
        let (_ring, out, mut input) = pair(4);
        out.write(&sample(1, 1.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);
    }

    #[test]
    fn test_drain_record_order() {
        let (_ring, out, mut input) = pair(8);
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
    fn test_latest_selects_newest_producer() {
        let (left, right) = (ring(4), ring(4));
        let a = Output::<Imu>::new(left.writer(NoWake).expect("free writer"));
        let b = Output::<Imu>::new(right.writer(NoWake).expect("free writer"));
        let mut input = Input::<Imu>::new(vec![
            left.view(NoWake).expect("free slot"),
            right.view(NoWake).expect("free slot"),
        ]);
        a.write(&sample(9, 1.0)).expect("ring has room");
        b.write(&sample(3, 2.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);

        b.write(&sample(11, 3.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 3.0);
    }

    #[test]
    fn test_latest_timestamp_tie() {
        let (left, right) = (ring(4), ring(4));
        let a = Output::<Imu>::new(left.writer(NoWake).expect("free writer"));
        let b = Output::<Imu>::new(right.writer(NoWake).expect("free writer"));
        let mut input = Input::<Imu>::new(vec![
            left.view(NoWake).expect("free slot"),
            right.view(NoWake).expect("free slot"),
        ]);
        a.write(&sample(5, 1.0)).expect("ring has room");
        b.write(&sample(5, 2.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);
    }

    #[test]
    fn test_drain_producer_order() {
        let (left, right) = (ring(8), ring(8));
        let a = Output::<Imu>::new(left.writer(NoWake).expect("free writer"));
        let b = Output::<Imu>::new(right.writer(NoWake).expect("free writer"));
        let mut input = Input::<Imu>::new(vec![
            left.view(NoWake).expect("free slot"),
            right.view(NoWake).expect("free slot"),
        ]);
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
    fn test_unconnected_input() {
        let mut input = Input::<Imu>::new(Vec::new());
        assert!(input.latest().expect("valid").is_none());
        assert_eq!(input.drain().count(), 0);
    }

    #[test]
    fn test_full_ring_blocks() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: 64,
            max_readers: 1,
        });
        let out = Output::new(ring.writer(NoWake).expect("free writer"));
        let mut input = Input::<Imu>::new(vec![ring.view(NoWake).expect("free slot")]);
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
    fn test_reject_short_record() {
        let ring = ring(4);
        let writer = ring.writer(NoWake).expect("free writer");
        let mut input = Input::<Imu>::new(vec![ring.view(NoWake).expect("free slot")]);
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
        let a = Output::new(left.writer(NoWake).expect("writer"));
        let b = Output::new(right.writer(NoWake).expect("writer"));
        let input = Input::new(vec![
            left.view(NoWake).expect("slot"),
            right.view(NoWake).expect("slot"),
        ]);
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

        fn schema() -> crate::record::RecordSchema {
            Stamped::schema()
        }
    }

    #[test]
    fn test_latest_decodes_once() {
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
    fn test_latest_into_inner_borrows_frame() {
        let (_ring, out, mut input) = pair(4);
        out.write(&sample(1, 2.0)).expect("fits");
        let frame: &Imu = input.latest().expect("valid").expect("record").into_inner();
        assert_eq!(frame.sample, 2.0);
        out.write(&sample(2, 3.0)).expect("fits");
        assert_eq!(frame.sample, 2.0);
    }

    #[test]
    fn test_latest_stamped_message_order() {
        let (_l, _r, a, b, mut input) = stamped_pair();
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
    fn test_latest_unstamped_message_order() {
        let (_l, _r, a, b, mut input) = message_pair_two::<Fixed>();
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
        let a = Output::new(left.writer(NoWake).expect("writer"));
        let b = Output::new(right.writer(NoWake).expect("writer"));
        let input = Input::new(vec![
            left.view(NoWake).expect("slot"),
            right.view(NoWake).expect("slot"),
        ]);
        (left, right, a, b, input)
    }

    fn message_pair<T: Record>() -> (RingBuffer, Output<T>, Input<T>) {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(T::MAX_LEN, 4).expect("valid capacity"),
            max_readers: 1,
        });
        let out = Output::new(ring.writer(NoWake).expect("free writer"));
        let input = Input::new(vec![ring.view(NoWake).expect("free slot")]);
        (ring, out, input)
    }

    #[test]
    fn test_message_round_trip() {
        let (_ring, out, mut input) = message_pair::<Note>();
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
    fn test_oversize_message_preserves_ring() {
        let (_ring, out, mut input) = message_pair::<Note>();
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
    fn test_drain_skips_corrupt_record() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(Fixed::MAX_LEN, 4).expect("valid capacity"),
            max_readers: 1,
        });
        let writer = ring.writer(NoWake).expect("free writer");
        let mut input = Input::<Fixed>::new(vec![ring.view(NoWake).expect("free slot")]);
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
    fn test_drain_frames_and_messages() {
        let (_ring, out, mut input) = pair(4);
        out.write(&sample(1, 1.0)).expect("ring has room");
        let borrowed: Option<&Imu> = input.drain().next().map(|r| r.expect("decodes"));
        assert_eq!(borrowed.map(|f| f.sample), Some(1.0));

        let (_ring, out, mut input) = message_pair::<Fixed>();
        out.write(&Fixed { a: 3, b: 0.5 }).expect("fits");
        let owned: Option<Fixed> = input.drain().next().map(|r| r.expect("decodes"));
        assert_eq!(owned, Some(Fixed { a: 3, b: 0.5 }));
    }

    #[test]
    fn test_shared_write_round_trips() {
        let (_ring, frames, mut frame_in) = pair(4);
        let shared: &Output<Imu> = &frames;
        shared.write(&sample(1, 4.0)).expect("ring has room");
        let frame = frame_in.latest().expect("valid").expect("record");
        assert_eq!(frame.sample, 4.0);

        let (_ring, notes, mut note_in) = message_pair::<Note>();
        let shared: &Output<Note> = &notes;
        shared.write(&Note { text: "hi".into() }).expect("fits");
        let seen: Vec<Note> = note_in.drain().map(|r| r.expect("decodes")).collect();
        assert_eq!(seen, [Note { text: "hi".into() }]);
    }

    #[test]
    fn test_failed_encode_returns_scratch() {
        let (_ring, out, mut input) = message_pair::<Note>();
        let long = Note {
            text: "x".repeat(100),
        };
        assert!(matches!(out.write(&long), Err(SendError::Encode(_))));
        out.write(&Note { text: "ok".into() })
            .expect("scratch is back");
        assert_eq!(input.drain().count(), 1);
    }

    #[test]
    fn test_closure_and_caller_write_in_order() {
        let (_ring, out, mut input) = pair(8);
        let send = |ts: i64| out.write(&sample(ts, ts as f64));
        send(1).expect("ring has room");
        out.write(&sample(2, 2.0)).expect("ring has room");
        send(3).expect("ring has room");
        let seen: Vec<f64> = input.drain().map(|r| r.expect("decodes").sample).collect();
        assert_eq!(seen, [1.0, 2.0, 3.0]);
    }

    /// What a [`Hostile`] record does while it encodes.
    enum Act<'o> {
        Nothing,
        Reenter(&'o Output<Hostile<'o>>),
        Panic,
    }

    /// A postcard record whose `encode` can write to its own port or panic.
    struct Hostile<'o> {
        text: String,
        act: Act<'o>,
        nested: core::cell::Cell<Option<Result<(), SendError>>>,
    }

    impl Hostile<'_> {
        fn new(text: &str) -> Self {
            Self {
                text: text.into(),
                act: Act::Nothing,
                nested: core::cell::Cell::new(None),
            }
        }
    }

    impl<'o> Record for Hostile<'o> {
        const NAME: &'static str = "hostile";
        const MAX_LEN: usize = 64;
        type Read<'a>
            = Self
        where
            Self: 'a;

        fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
            match self.act {
                Act::Nothing => {}
                Act::Reenter(port) => self.nested.set(Some(port.write(&Hostile::new("inner")))),
                Act::Panic => panic!("encode failed"),
            }
            crate::record::postcard::encode(&self.text, buf)
        }

        fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
            let text: String = crate::record::postcard::decode(bytes)?;
            Ok(Self::new(&text))
        }

        fn schema() -> crate::record::RecordSchema {
            Note::schema()
        }
    }

    fn texts(input: &mut Input<Hostile<'_>>) -> Vec<String> {
        input.drain().map(|r| r.expect("decodes").text).collect()
    }

    #[test]
    fn test_reentrant_write_is_busy() {
        let (_ring, out, mut input) = message_pair::<Hostile<'_>>();
        let outer = Hostile {
            act: Act::Reenter(&out),
            ..Hostile::new("outer")
        };
        out.write(&outer).expect("outer write lands");
        assert_eq!(outer.nested.get(), Some(Err(SendError::Busy)));
        out.write(&Hostile::new("after")).expect("scratch is back");
        assert_eq!(texts(&mut input), ["outer", "after"]);
    }

    #[test]
    fn test_panicking_encode_returns_scratch() {
        let (_ring, out, mut input) = message_pair::<Hostile<'_>>();
        let bad = Hostile {
            act: Act::Panic,
            ..Hostile::new("bad")
        };
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| out.write(&bad)));
        assert!(caught.is_err());
        out.write(&Hostile::new("after")).expect("scratch is back");
        assert_eq!(texts(&mut input), ["after"]);
    }
}

#[cfg(test)]
mod async_tests {
    use core::future::Future;
    use core::task::{Context, Poll};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Wake, Waker};

    use futures_lite::future::poll_once;
    use metor_fsw_3_ring::{Config, Notifier, RingBuffer};

    use super::*;
    use crate::system::InputBinding;
    use crate::tests::utils::Imu;

    #[derive(Default)]
    struct Woken(AtomicBool);

    impl Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    type WokenInputs = (
        [RingBuffer; 2],
        [Output<Imu, Notifier>; 2],
        Input<Imu, Notifier>,
    );

    fn independently_woken() -> WokenInputs {
        let (left, a, first) = woken(&Notifier::default());
        let (right, b, second) = woken(&Notifier::default());
        let views = first.views.into_iter().chain(second.views).collect();
        let input = Input::new(views);
        ([left, right], [a, b], input)
    }

    /// One ring holding four `Imu` records, woken through `wake`.
    fn woken(wake: &Notifier) -> (RingBuffer, Output<Imu, Notifier>, Input<Imu, Notifier>) {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(Imu::MAX_LEN, 4).expect("valid capacity"),
            max_readers: 1,
        });
        let out = Output::new(ring.writer(wake.clone()).expect("free writer"));
        let input = Input::new(vec![ring.view(wake.clone()).expect("free slot")]);
        (ring, out, input)
    }

    #[stellarator::test]
    async fn test_next_waits_for_record() {
        let wake = Notifier::default();
        let (_ring, out, mut input) = woken(&wake);
        assert!(poll_once(input.next()).await.is_none());
        out.write(&Imu::new(1, 1.0)).expect("ring has room");
        out.write(&Imu::new(2, 2.0)).expect("ring has room");
        assert_eq!(input.next().await.expect("record").sample, 1.0);
        assert_eq!(input.next().await.expect("record").sample, 2.0);
        assert!(poll_once(input.next()).await.is_none());
    }

    #[stellarator::test]
    async fn test_next_without_producer() {
        let mut input = Input::<Imu, Notifier>::new(Vec::new());
        assert!(poll_once(input.next()).await.is_none());
    }

    #[test]
    fn test_next_wakes_per_producer() {
        let (_rings, outputs, mut input) = independently_woken();
        let woken = Arc::new(Woken::default());
        let waker = Waker::from(woken.clone());
        let mut cx = Context::from_waker(&waker);

        for at in [1, 0, 1, 0] {
            let mut next = core::pin::pin!(input.next());
            assert!(next.as_mut().poll(&mut cx).is_pending());
            woken.0.store(false, Ordering::SeqCst);
            outputs[at].write(&Imu::new(1, at as f64)).expect("room");
            assert!(woken.0.load(Ordering::SeqCst));
            let Poll::Ready(Ok(record)) = next.as_mut().poll(&mut cx) else {
                panic!("the notified producer has a record")
            };
            assert_eq!(record.sample, at as f64);
        }
    }

    #[test]
    fn test_next_cancel_and_retry() {
        let (_rings, outputs, mut input) = independently_woken();
        let wakes: Vec<_> = input.views.iter().map(|view| view.wake().clone()).collect();
        let woken = Arc::new(Woken::default());
        let waker = Waker::from(woken.clone());
        let mut cx = Context::from_waker(&waker);
        {
            let mut next = core::pin::pin!(input.next());
            assert!(next.as_mut().poll(&mut cx).is_pending());
        }
        for wake in &wakes {
            wake.notify();
        }
        assert!(!woken.0.load(Ordering::SeqCst));

        let mut next = core::pin::pin!(input.next());
        assert!(next.as_mut().poll(&mut cx).is_pending());
        outputs[1].write(&Imu::new(1, 7.0)).expect("room");
        assert!(woken.0.load(Ordering::SeqCst));
        assert!(matches!(next.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    }

    #[test]
    fn test_empty_notification_rearms_waiter() {
        let wake = Notifier::default();
        let (_ring, output, mut input) = woken(&wake);
        let woken = Arc::new(Woken::default());
        let waker = Waker::from(woken.clone());
        let mut cx = Context::from_waker(&waker);
        let mut next = core::pin::pin!(input.next());
        assert!(next.as_mut().poll(&mut cx).is_pending());
        wake.notify();
        woken.0.store(false, Ordering::SeqCst);
        assert!(next.as_mut().poll(&mut cx).is_pending());
        assert!(
            woken.0.load(Ordering::SeqCst),
            "rearming schedules another poll"
        );
        assert!(next.as_mut().poll(&mut cx).is_pending());
        woken.0.store(false, Ordering::SeqCst);
        output.write(&Imu::new(1, 7.0)).expect("room");
        assert!(woken.0.load(Ordering::SeqCst));
        assert!(matches!(next.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    }

    #[stellarator::test]
    async fn test_next_consumes_corrupt_record() {
        let (_ring, output, mut input) = woken(&Notifier::default());
        output.writer.try_write(&[0]).expect("room");
        assert!(matches!(input.next().await, Err(RecvError::Decode(_))));
        output.write(&Imu::new(2, 7.0)).expect("room");
        assert_eq!(input.next().await.expect("valid record").sample, 7.0);
    }

    #[stellarator::test]
    async fn test_write_wakes_port_across_threads() {
        let wake = Notifier::default();
        let (ring, out, mut input) = woken(&wake);
        drop(out);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(core::time::Duration::from_millis(20));
            let out = Output::<Imu, _>::new(ring.writer(wake).expect("free writer"));
            out.write(&Imu::new(7, 7.0)).expect("ring has room");
            ring
        });
        assert_eq!(input.next().await.expect("record").sample, 7.0);
        let _ring = writer.join().expect("the writer thread finished");
    }

    #[stellarator::test]
    async fn test_any_ready_waits_for_record() {
        let wake = Notifier::default();
        let (_left, a, _) = woken(&wake);
        let (right, _, _) = woken(&wake);
        let mut inputs = DynInputs::<Notifier>::bind(vec![
            InputBinding {
                def: Input::<Imu>::def(&DefCx::empty(), "a.imu"),
                views: Vec::new(),
            },
            InputBinding {
                def: Input::<Imu>::def(&DefCx::empty(), "b.imu"),
                views: vec![right.view(wake.clone()).expect("free slot")],
            },
        ]);
        assert!(poll_once(inputs.any_ready()).await.is_none());
        a.write(&Imu::new(1, 1.0)).expect("ring has room");
        // The record landed on a ring no port reads, so nothing is ready.
        assert!(poll_once(inputs.any_ready()).await.is_none());
        let out = Output::<Imu, _>::new(right.writer(wake).expect("free writer"));
        out.write(&Imu::new(2, 2.0)).expect("ring has room");
        inputs.any_ready().await;
        let seen: Vec<_> = inputs
            .iter_mut()
            .flat_map(|(def, input)| input.drain().map(move |r| (def.name.to_string(), r)))
            .map(|(name, bytes)| (name, bytes.expect("bytes").len()))
            .collect();
        assert_eq!(seen, vec![("b.imu".to_string(), Imu::MAX_LEN)]);
    }

    fn independent_ports() -> (
        [RingBuffer; 2],
        [Output<Imu, Notifier>; 2],
        DynInputs<Notifier>,
    ) {
        let (left, a, first) = woken(&Notifier::default());
        let (right, b, second) = woken(&Notifier::default());
        let inputs = DynInputs::bind(vec![
            InputBinding {
                def: Input::<Imu>::def(&DefCx::empty(), "a"),
                views: first.views,
            },
            InputBinding {
                def: Input::<Imu>::def(&DefCx::empty(), "b"),
                views: second.views,
            },
        ]);
        ([left, right], [a, b], inputs)
    }

    #[test]
    fn test_any_ready_wakes_per_port() {
        let (_rings, outputs, mut inputs) = independent_ports();
        let woken = Arc::new(Woken::default());
        let waker = Waker::from(woken.clone());
        let mut cx = Context::from_waker(&waker);
        for at in [1, 0, 1, 0] {
            {
                let mut ready = core::pin::pin!(inputs.any_ready());
                assert!(ready.as_mut().poll(&mut cx).is_pending());
                woken.0.store(false, Ordering::SeqCst);
                outputs[at].write(&Imu::new(1, 7.0)).expect("room");
                assert!(woken.0.load(Ordering::SeqCst));
                assert!(ready.as_mut().poll(&mut cx).is_ready());
            }
            let count = inputs
                .iter_mut()
                .flat_map(|(_, input)| input.drain())
                .count();
            assert_eq!(count, 1);
        }
    }

    #[test]
    fn test_any_ready_cancel_and_retry() {
        let (_rings, outputs, mut inputs) = independent_ports();
        let wakes: Vec<_> = inputs
            .ports
            .iter()
            .flat_map(|(_, input)| input.views.iter().map(|view| view.wake().clone()))
            .collect();
        let woken = Arc::new(Woken::default());
        let waker = Waker::from(woken.clone());
        let mut cx = Context::from_waker(&waker);
        {
            let mut ready = core::pin::pin!(inputs.any_ready());
            assert!(ready.as_mut().poll(&mut cx).is_pending());
        }
        for wake in &wakes {
            wake.notify();
        }
        assert!(!woken.0.load(Ordering::SeqCst));

        let mut ready = core::pin::pin!(inputs.any_ready());
        assert!(ready.as_mut().poll(&mut cx).is_pending());
        outputs[1].write(&Imu::new(1, 7.0)).expect("room");
        assert!(woken.0.load(Ordering::SeqCst));
        assert!(ready.as_mut().poll(&mut cx).is_ready());
    }

    #[stellarator::test]
    async fn test_any_ready_without_producers() {
        let mut empty = DynInputs::<Notifier>::default();
        assert!(poll_once(empty.any_ready()).await.is_none());
        let mut unbound = DynInputs::<Notifier>::bind(vec![InputBinding {
            def: Input::<Imu>::def(&DefCx::empty(), "unbound"),
            views: Vec::new(),
        }]);
        assert!(poll_once(unbound.any_ready()).await.is_none());
    }
}
