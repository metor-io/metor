//! Typed ports that systems read and write.
//!
//! A port wraps ring handles and encodes or decodes records of one type. An
//! [`Output`] writes to one ring. An [`Input`] reads from every ring that feeds
//! it, one per producer.

use core::borrow::Borrow;
use core::cell::Cell;
use core::marker::PhantomData;
use core::ops::Deref;
use core::sync::atomic::{AtomicU32, Ordering};
use std::borrow::Cow;
use std::sync::Arc;

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
pub(crate) use sample::Samplers;

/// Returns the ring capacity needed to hold `depth` records.
///
/// Each record is at most `max_len` bytes. The capacity is a power of two and
/// always fits at least two records. Returns `None` on overflow.
pub fn ring_capacity(max_len: usize, depth: usize) -> Option<usize> {
    u32::try_from(max_len).ok()?;
    max_len.checked_add(2 * PAYLOAD_ALIGNMENT - 1)?;
    frame_len(max_len)
        .checked_mul(depth.max(2))?
        .checked_next_power_of_two()
}

/// An error from writing to an [`Output`].
///
/// When a write returns an error, no record was published.
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

/// An error from reading an [`Input`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecvError {
    #[error(transparent)]
    Ring(ReadError),
    #[error(transparent)]
    Decode(DecodeError),
}

/// Rejects a record longer than `max` before it reaches the ring.
fn bounded(bytes: &[u8], max: usize) -> Result<&[u8], SendError> {
    if bytes.len() > max {
        return Err(SendError::Oversize {
            len: bytes.len(),
            max,
        });
    }
    Ok(bytes)
}

/// A scratch buffer borrowed for one write.
///
/// Dropping it puts the buffer back in its cell.
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

/// Publishes records of type `T` to one ring.
///
/// `W` is notified after each write. System outputs use [`NoWake`]. Adapters
/// that feed async inputs use [`Notifier`], so the async system wakes up.
pub struct Output<T: ?Sized, W: WakeSource = NoWake> {
    writer: Writer<W>,
    /// The encode buffer.
    ///
    /// It is empty while a write is using it.
    scratch: Cell<Option<Box<[u8]>>>,
    /// The longest record this port accepts.
    ///
    /// For a typed port this is `T::MAX_LEN`. A dynamic port does not know its
    /// record type, so its binding sets the limit.
    max_len: usize,
    /// The count of failed writes, shared with the system's other outputs.
    dropped: Arc<AtomicU32>,
    _t: PhantomData<T>,
}

impl<T: ?Sized, W: WakeSource> Output<T, W> {
    /// Counts a failed write and returns `result` unchanged.
    ///
    /// The count saturates instead of wrapping.
    fn counted(&self, result: Result<(), SendError>) -> Result<(), SendError> {
        if result.is_err() {
            let _ = self
                .dropped
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1));
        }
        result
    }
}

impl<T: Record + ?Sized, W: WakeSource> Output<T, W> {
    /// Creates an output that writes through `writer`.
    ///
    /// A record type that needs more alignment than the ring provides fails to
    /// compile.
    pub fn new(writer: Writer<W>) -> Self {
        Self::counting(writer, Arc::default())
    }

    /// Creates an output that counts its failed writes in `dropped`.
    pub fn counting(writer: Writer<W>, dropped: Arc<AtomicU32>) -> Self {
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
            dropped,
            _t: PhantomData,
        }
    }

    /// Returns the definition of an output port called `name`.
    ///
    /// The record's schema is rooted at `cx.port_path(name)`.
    pub fn def(cx: &DefCx<'_>, name: &'static str) -> PortDef {
        PortDef {
            name: name.into(),
            record: T::NAME.into(),
            id: T::ID,
            max_len: T::MAX_LEN,
            alignment: T::ALIGN,
            depth: T::DEPTH,
            layout: T::schema().layout(),
            schema: T::schema_at(&cx.port_path(name)),
        }
    }

    /// Encodes `value` and publishes it as one record.
    ///
    /// A failure is counted as well as returned. If `T::encode` itself writes
    /// to this same port, that inner write fails with [`SendError::Busy`].
    pub fn write(&self, value: &T) -> Result<(), SendError> {
        self.counted(self.encode_write(value))
    }

    fn encode_write(&self, value: &T) -> Result<(), SendError> {
        let mut scratch = Lent::take(&self.scratch)?;
        let bytes = value.encode(&mut scratch.buf).map_err(SendError::Encode)?;
        let bytes = bounded(bytes, self.max_len)?;
        self.writer.try_write(bytes).map_err(SendError::Ring)
    }
}

impl<W: WakeSource> Output<Bytes, W> {
    /// Creates a dynamic output that writes raw bytes.
    ///
    /// Records longer than `max_len` are rejected.
    pub fn bytes(writer: Writer<W>, max_len: usize) -> Self {
        Self::with_dropped_counter(writer, max_len, Arc::default())
    }

    /// Creates a dynamic output that counts its failed writes in `dropped`.
    pub fn with_dropped_counter(
        writer: Writer<W>,
        max_len: usize,
        dropped: Arc<AtomicU32>,
    ) -> Self {
        Self {
            writer,
            scratch: Cell::new(Some(Box::default())),
            max_len,
            dropped,
            _t: PhantomData,
        }
    }

    /// Publishes already-encoded bytes as one record.
    ///
    /// A failure is counted as well as returned.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), SendError> {
        let written = bounded(bytes, self.max_len)
            .and_then(|bytes| self.writer.try_write(bytes).map_err(SendError::Ring));
        self.counted(written)
    }
}

/// Reads records of type `T` from every producer feeding one port.
///
/// Each producer writes its own ring, and the input holds one view per ring.
/// `W` is what those views wait on. An async input wakes when any producer
/// writes, whether the producers share one [`Notifier`] or each have their own.
pub struct Input<T: ?Sized, W: WakeSink = NoWake> {
    views: Vec<View<W>>,
    waiters: Waiters,
    _t: PhantomData<T>,
}

impl<T: Record + ?Sized, W: WakeSink> Input<T, W> {
    /// Creates an input that reads from `views`, one per producer.
    ///
    /// A record type that needs more alignment than the ring provides fails to
    /// compile.
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

    /// Returns the definition of an input port called `name`.
    pub fn def(cx: &DefCx<'_>, name: &'static str) -> PortDef {
        Output::<T>::def(cx, name)
    }

    /// Reads every unread record.
    ///
    /// Producers are read one at a time, in the order they were bound.
    pub fn drain(&mut self) -> impl Iterator<Item = Result<T::Read<'_>, RecvError>> + '_ {
        self.views.iter_mut().flat_map(|view| {
            view.drain().map(|record| {
                let bytes = record.map_err(RecvError::Ring)?;
                T::decode(bytes).map_err(RecvError::Decode)
            })
        })
    }

    /// Returns the newest unread record across all producers.
    ///
    /// Records are compared by timestamp. On a tie, or when records have no
    /// timestamp, the earliest-bound producer wins. Older records are skipped.
    /// The returned record stays unread, so the next call returns it again
    /// until a newer one arrives.
    pub fn latest(&mut self) -> Result<Option<Latest<'_, T>>, RecvError> {
        let mut best: Option<(Option<Timestamp>, T::Read<'_>)> = None;
        for view in self.views.iter_mut() {
            let Some(bytes) = view.try_latest().map_err(RecvError::Ring)? else {
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

    /// Returns the index of the first producer with an unread record.
    ///
    /// Deferred drain consumption is applied first.
    fn ready(&mut self) -> Option<usize> {
        self.views.iter_mut().position(|view| {
            view.settle();
            view.has_record()
        })
    }
}

impl<T: Record + ?Sized> Input<T, Notifier> {
    /// Waits for the next record and reads it.
    ///
    /// If several producers have records, the earliest-bound one is read first.
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

/// Returns definitions for config inputs that no declared port claimed.
///
/// Each port takes its name from the config and its record from its producer.
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

/// Returns definitions for config outputs that no declared port claimed.
///
/// Each port's record comes from the record the config names, or from the
/// definition `metor build` resolved for it.
pub fn dyn_output_defs(
    cx: &DefCx<'_>,
    declared: &[Cow<'static, str>],
) -> Result<Vec<PortDef>, DefError> {
    cx.outputs
        .iter()
        .filter(|output| !declared.iter().any(|taken| *taken == output.port))
        .map(|output| match &output.source {
            OutputSource::Record(record) => cx.records.port_def(&output.port, record),
            OutputSource::Def(def) => Ok(PortDef {
                name: output.port.clone().into(),
                ..def.clone()
            }),
        })
        .collect()
}

/// Inputs for a system whose ports are listed in config instead of code.
///
/// Records arrive as raw bytes, undecoded. Each port comes with the definition
/// computed from the config, so the system can tell what it carries.
pub struct DynInputs<W: WakeSink = NoWake> {
    ports: Vec<(PortDef, Input<Bytes, W>)>,
}

impl<W: WakeSink> DynInputs<W> {
    /// Returns each port's definition alongside its input.
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

/// Outputs for a system whose ports are listed in config instead of code.
pub struct DynOutputs<W: WakeSource = NoWake> {
    ports: Vec<(PortDef, Output<Bytes, W>)>,
}

impl<W: WakeSource> DynOutputs<W> {
    /// Creates the outputs, counting their failed writes in `dropped`.
    pub fn new(outputs: Vec<OutputBinding<W>>, dropped: &Arc<AtomicU32>) -> Self {
        let ports = outputs
            .into_iter()
            .map(|b| {
                let output = Output::with_dropped_counter(b.writer, b.def.max_len, dropped.clone());
                (b.def, output)
            })
            .collect();
        Self { ports }
    }

    /// Returns each port's definition alongside its output.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&PortDef, &mut Output<Bytes, W>)> {
        self.ports.iter_mut().map(|(def, output)| (&*def, output))
    }
}

impl<W: WakeSource> SystemOutputs<W> for DynOutputs<W> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        dyn_output_defs(cx, &[])
    }

    fn bind(outputs: Vec<OutputBinding<W>>) -> Self {
        Self::new(outputs, &Arc::default())
    }
}

/// The newest record, returned by [`Input::latest`].
///
/// It derefs to the decoded record.
pub struct Latest<'a, T: Record + ?Sized + 'a> {
    decoded: T::Read<'a>,
}

impl<'a, T: Record + ?Sized> Latest<'a, T> {
    /// Returns the decoded record.
    ///
    /// A frame is borrowed from the ring. A message is decoded into an owned value.
    pub fn into_inner(self) -> T::Read<'a> {
        self.decoded
    }
}

impl<T: Record + ?Sized> Deref for Latest<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.decoded.borrow()
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
    use crate::tests::utils::{Fixed, Imu, Note, Stamped, input, output, pair, ring};
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
        })
        .expect("a valid ring config");
        let output = output::<Aligned>(&ring);
        let mut input = input::<Aligned>(&ring);
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

    /// Two one-reader rings of `depth` `T` records feeding one input.
    fn fan_in<T: Record + ?Sized>(
        depth: usize,
    ) -> (RingBuffer, RingBuffer, Output<T>, Output<T>, Input<T>) {
        let (left, right) = (ring::<T>(depth, 1), ring::<T>(depth, 1));
        let (a, b) = (output(&left), output(&right));
        let views = [&left, &right].map(|ring| ring.view(NoWake).expect("free slot"));
        (left, right, a, b, Input::new(views.into()))
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
        let (_ring, out, mut input) = pair::<Imu>(4);
        out.write(&Imu::new(7, 42.0)).expect("ring has room");
        let got = input.latest().expect("valid record").expect("one record");
        assert_eq!(got.sample, 42.0);
        assert_eq!(got.timestamp, Timestamp(7));
    }

    #[test]
    fn test_latest_repeats_pinned_record() {
        let (_ring, out, mut input) = pair::<Imu>(4);
        out.write(&Imu::new(1, 1.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);
    }

    #[test]
    fn test_drain_record_order() {
        let (_ring, out, mut input) = pair::<Imu>(8);
        for i in 0..3 {
            out.write(&Imu::new(i, i as f64)).expect("ring has room");
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
        let (_l, _r, a, b, mut input) = fan_in::<Imu>(4);
        a.write(&Imu::new(9, 1.0)).expect("ring has room");
        b.write(&Imu::new(3, 2.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);

        b.write(&Imu::new(11, 3.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 3.0);
    }

    #[test]
    fn test_latest_timestamp_tie() {
        let (_l, _r, a, b, mut input) = fan_in::<Imu>(4);
        a.write(&Imu::new(5, 1.0)).expect("ring has room");
        b.write(&Imu::new(5, 2.0)).expect("ring has room");
        assert_eq!(input.latest().expect("valid").expect("record").sample, 1.0);
    }

    #[test]
    fn test_drain_producer_order() {
        let (_l, _r, a, b, mut input) = fan_in::<Imu>(8);
        b.write(&Imu::new(0, 10.0)).expect("ring has room");
        a.write(&Imu::new(1, 1.0)).expect("ring has room");
        a.write(&Imu::new(2, 2.0)).expect("ring has room");
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
        })
        .expect("a valid ring config");
        let out = output(&ring);
        let mut input = input::<Imu>(&ring);
        out.write(&Imu::new(0, 0.0)).expect("ring has room");
        let pinned = input.latest().expect("valid").expect("record");
        assert_eq!(pinned.sample, 0.0);
        // Two 32-byte records fill the ring.
        out.write(&Imu::new(1, 1.0)).expect("ring has room");
        assert_eq!(
            out.write(&Imu::new(2, 2.0)),
            Err(SendError::Ring(WriteError::WouldBlock))
        );
    }

    #[test]
    fn test_full_ring_write_counts() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: 64,
            max_readers: 1,
        })
        .expect("a valid ring config");
        let dropped = Arc::new(AtomicU32::new(0));
        let writer = ring.writer(NoWake).expect("free writer");
        let out = Output::<Imu>::counting(writer, dropped.clone());
        let _input = input::<Imu>(&ring);
        out.write(&Imu::new(0, 0.0)).expect("ring has room");
        out.write(&Imu::new(1, 1.0)).expect("ring has room");
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert_eq!(
            out.write(&Imu::new(2, 2.0)),
            Err(SendError::Ring(WriteError::WouldBlock))
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        dropped.store(u32::MAX, Ordering::Relaxed);
        assert!(out.write(&Imu::new(3, 3.0)).is_err());
        assert_eq!(dropped.load(Ordering::Relaxed), u32::MAX, "saturates");
    }

    #[test]
    fn test_oversize_write_counts() {
        let ring = ring::<Imu>(8, 1);
        let dropped = Arc::new(AtomicU32::new(0));
        let writer = ring.writer(NoWake).expect("free writer");
        let out = Output::with_dropped_counter(writer, 4, dropped.clone());
        assert_eq!(
            out.write_bytes(&[0; 5]),
            Err(SendError::Oversize { len: 5, max: 4 })
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        out.write_bytes(&[0; 4]).expect("fits");
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_reject_short_record() {
        let ring = ring::<Imu>(4, 1);
        let writer = ring.writer(NoWake).expect("free writer");
        let mut input = input::<Imu>(&ring);
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
        let (_l, _r, mut a, mut b, mut input) = fan_in::<Counted>(4);
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
        assert_eq!(latest.0.at, Timestamp(2));
        assert!(core::ptr::eq(&*latest, &*latest));
        let owned: Counted = latest.into_inner();
        assert_eq!(owned.0.text, "sample");
        assert_eq!(DECODES.get(), 2);
    }

    #[test]
    fn test_latest_into_inner_borrows_frame() {
        let (_ring, out, mut input) = pair::<Imu>(4);
        out.write(&Imu::new(1, 2.0)).expect("fits");
        let frame: &Imu = input.latest().expect("valid").expect("record").into_inner();
        assert_eq!(frame.sample, 2.0);
        out.write(&Imu::new(2, 3.0)).expect("fits");
        assert_eq!(frame.sample, 2.0);
    }

    #[test]
    fn test_latest_stamped_message_order() {
        let (_l, _r, a, b, mut input) = fan_in::<Stamped>(4);
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
        assert_eq!(input.latest().unwrap().unwrap().text, "a");
        b.write(&Stamped {
            at: Timestamp(11),
            text: "b2".into(),
        })
        .expect("fits");
        let newest = input.latest().unwrap().unwrap();
        assert_eq!(newest.text, "b2");
        assert_eq!(newest.at, Timestamp(11));
    }

    #[test]
    fn test_latest_unstamped_message_order() {
        let (_l, _r, a, b, mut input) = fan_in::<Fixed>(4);
        b.write(&Fixed { a: 2, b: 0.0 }).expect("fits");
        a.write(&Fixed { a: 1, b: 0.0 }).expect("fits");
        assert_eq!(input.latest().unwrap().unwrap().a, 1);
    }

    #[test]
    fn test_message_round_trip() {
        let (_ring, out, mut input) = pair::<Note>(4);
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
        let (_ring, out, mut input) = pair::<Note>(4);
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
        let ring = ring::<Fixed>(4, 1);
        let writer = ring.writer(NoWake).expect("free writer");
        let mut input = input::<Fixed>(&ring);
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
        let (_ring, out, mut input) = pair::<Imu>(4);
        out.write(&Imu::new(1, 1.0)).expect("ring has room");
        let borrowed: Option<&Imu> = input.drain().next().map(|r| r.expect("decodes"));
        assert_eq!(borrowed.map(|f| f.sample), Some(1.0));

        let (_ring, out, mut input) = pair::<Fixed>(4);
        out.write(&Fixed { a: 3, b: 0.5 }).expect("fits");
        let owned: Option<Fixed> = input.drain().next().map(|r| r.expect("decodes"));
        assert_eq!(owned, Some(Fixed { a: 3, b: 0.5 }));
    }

    #[test]
    fn test_failed_encode_returns_scratch() {
        let (_ring, out, mut input) = pair::<Note>(4);
        let long = Note {
            text: "x".repeat(100),
        };
        assert!(matches!(out.write(&long), Err(SendError::Encode(_))));
        out.write(&Note { text: "ok".into() })
            .expect("scratch is back");
        assert_eq!(input.drain().count(), 1);
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
        let (_ring, out, mut input) = pair::<Hostile<'_>>(4);
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
        let (_ring, out, mut input) = pair::<Hostile<'_>>(4);
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

#[cfg(all(test, feature = "host"))]
mod async_tests {
    use core::future::Future;
    use core::task::{Context, Poll};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Wake, Waker};

    use futures_lite::future::poll_once;
    use metor_fsw_3_ring::{Notifier, RingBuffer};

    use super::*;
    use crate::system::InputBinding;
    use crate::tests::utils::{Imu, ring};

    #[derive(Default)]
    struct Woken(AtomicBool);

    impl Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A waker that records being woken in the flag beside it.
    fn waker() -> (Arc<Woken>, Waker) {
        let woken = Arc::new(Woken::default());
        (woken.clone(), Waker::from(woken))
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
        let ring = ring::<Imu>(4, 1);
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
        let (woken, waker) = waker();
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
        let (woken, waker) = waker();
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
        let (woken, waker) = waker();
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

    #[stellarator::test]
    async fn test_any_ready_without_producers() {
        let mut empty = DynInputs::<Notifier>::bind(Vec::new());
        assert!(poll_once(empty.any_ready()).await.is_none());
        let mut unbound = DynInputs::<Notifier>::bind(vec![InputBinding {
            def: Input::<Imu>::def(&DefCx::empty(), "unbound"),
            views: Vec::new(),
        }]);
        assert!(poll_once(unbound.any_ready()).await.is_none());
    }
}
