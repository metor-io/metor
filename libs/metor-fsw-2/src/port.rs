//! Typed ports over ring handles.
//!
//! A system exchanges frames through ports. [`Output<F>`] wraps the single
//! ring [`Writer`] a system owns for frame `F`, and [`Input<F>`] wraps a
//! read-only [`View`] of some upstream output's ring. Both wrappers are thin
//! because a frame's table bytes are the ring payload. The fixed `#[repr(C)]`
//! region sits at offset 0, so a fixed-frame write is one `try_write` of
//! `frame.as_bytes()` and a read is a zero-copy borrow of the record in
//! place, handed out as a typed [`FrameRef`] or [`FrameGrant`].
//!
//! Inputs support two consumption patterns. [`Input::latest`] serves cyclic
//! systems that want the freshest sample. It consumes any backlog but leaves
//! the newest record pinned on the ring, so a cycle with no new data sees the
//! same record again and the writer waits rather than overwrite it.
//! [`Input::drain`] serves command and event channels that must see every
//! record, handing each one to a callback in order and freeing it as soon as
//! the callback returns.
//!
//! On the write side, [`Output::write`] returns the ring error so callers can
//! react to a full ring, while [`Output::publish`] never fails the cycle. A
//! failed publish means either the ring was undersized or a slow reader is
//! holding it full, and in both cases the port counts the dropped record so
//! the runner can surface the count as a health error via
//! [`Output::take_dropped`].

use core::marker::PhantomData;

use metor_fsw::Decomponentize;
use metor_fsw_ring::{NoWake, ReadError, ReadGrant, View, WakeSink, WakeSource, Writer, frame_len};
use metor_proto::error::Error as ProtoError;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::binder::RingSource;
use crate::descriptor::{PortDecl, PortDesc};
use crate::dynamic::Slot;
use crate::frame::Frame;
use crate::reader::{ListReader, MapReader};
use crate::writer::{FrameScratch, FrameWriter};

/// Default in-flight record depth for a port. Must be at least 2, since a
/// latest-wins consumer permanently pins the newest record (see
/// [`Input::latest`]) and the writer still needs room for one more.
pub const DEFAULT_DEPTH: usize = 8;

/// Power-of-two ring capacity for `depth` records of at most `max_size` table
/// bytes each. `frame_len` accounts for the record header and payload padding.
pub fn capacity_for(max_size: usize, depth: usize) -> usize {
    (frame_len(max_size) * depth.max(2)).next_power_of_two()
}

/// As [`capacity_for`], reading the worst-case size from the frame type.
pub fn buffer_capacity<F: Frame>(depth: usize) -> usize {
    capacity_for(F::MAX_SIZE, depth)
}

/// Hand `f` a zero-copy borrow of every committed record since the last
/// drain, in order. Each grant drops, freeing its record for the writer,
/// before the next is taken. [`ReadError::Corrupt`] stops the drain and
/// propagates.
pub(crate) fn drain_view<RD, RS>(
    view: &mut View<RD, RS>,
    mut f: impl FnMut(&[u8]),
) -> Result<(), ReadError>
where
    RD: WakeSink,
    RS: WakeSource,
{
    loop {
        match view.try_read()? {
            Some(grant) => f(&grant),
            None => return Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Where a port's dropped-publish count accumulates. A runner-owned port
/// counts locally and the runner drains it with `take_dropped`; a port moved
/// into a future counts through a cell shared with the driving wrapper,
/// which folds it into health — the counters stay reachable either way.
/// Only the failure path and the per-cycle drain touch it.
pub(crate) enum Drops {
    Local(u64),
    Shared(std::sync::Arc<core::sync::atomic::AtomicU64>),
}

impl Drops {
    pub(crate) fn bump(&mut self) {
        use core::sync::atomic::Ordering::Relaxed;
        match self {
            Drops::Local(n) => *n += 1,
            Drops::Shared(cell) => {
                cell.fetch_add(1, Relaxed);
            }
        }
    }

    /// The local count, taken; a shared counter reports zero here (its owner
    /// drains the cell directly).
    pub(crate) fn take_local(&mut self) -> u64 {
        match self {
            Drops::Local(n) => core::mem::take(n),
            Drops::Shared(_) => 0,
        }
    }

    /// Switch to a shared cell, folding any local count in.
    pub(crate) fn share(&mut self, cell: std::sync::Arc<core::sync::atomic::AtomicU64>) {
        use core::sync::atomic::Ordering::Relaxed;
        if let Drops::Local(n) = self {
            cell.fetch_add(*n, Relaxed);
        }
        *self = Drops::Shared(cell);
    }
}

/// An output publishes a system's frames of type `F`, wrapping the single
/// [`Writer`] into the ring that carries them to downstream [`Input`]s.
/// Cyclic outputs default to [`NoWake`]; async outputs pick wake endpoints
/// so a write can suspend for space.
pub struct Output<F, WD = NoWake, WS = NoWake>
where
    WD: WakeSource,
    WS: WakeSink,
{
    writer: Writer<WD, WS>,
    /// Reused table-packet + key-staging backing for
    /// [`write_with`](Self::write_with), so a per-cycle publish of a dynamic
    /// frame allocates nothing after warmup. `None` until the first such
    /// write.
    scratch: Option<FrameScratch>,
    /// Records dropped by the infallible [`publish`](Self::publish) path,
    /// collected by the runner via [`take_dropped`](Self::take_dropped) or a
    /// future-owning driver's shared cell.
    dropped: Drops,
    _f: PhantomData<F>,
}

impl<F: Frame, WD: WakeSource, WS: WakeSink> Output<F, WD, WS> {
    /// Wrap a writer created over an `F`-sized buffer.
    pub fn new(writer: Writer<WD, WS>) -> Self {
        Self {
            writer,
            scratch: None,
            dropped: Drops::Local(0),
            _f: PhantomData,
        }
    }

    /// This port's static descriptor, for wiring and sizing before any data
    /// flows.
    pub fn descriptor() -> PortDesc {
        PortDesc::of::<F>()
    }

    /// This field's entry in the bundle's [`decls`](crate::SystemOutput::decls)
    /// walk, an ordinary wired port.
    pub fn decl() -> PortDecl {
        PortDecl::Port(Self::descriptor())
    }

    /// Take and clear the [`publish`](Self::publish) drop counter.
    pub fn take_dropped(&mut self) -> u64 {
        self.dropped.take_local()
    }

    /// Count drops through `cell` instead of locally, for a port about to
    /// move into a future (whose driver folds the cell into health).
    pub(crate) fn share_drops(&mut self, cell: std::sync::Arc<core::sync::atomic::AtomicU64>) {
        self.dropped.share(cell);
    }
}

impl<F: Frame, WD, WS> Output<F, WD, WS>
where
    WD: WakeSource + Default + Clone + 'static,
    WS: WakeSink + Default + Clone + 'static,
{
    /// Bind this output over the next ring the [`RingSource`] hands out,
    /// taking the writer-side wake endpoints matched to that buffer. The
    /// generated bundle `bind` walks fields in `descriptors()` order.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        let (ring, data, space) = src.next_output::<WD, WS>();
        // One ring is allocated per output port and bound exactly once, so
        // the region's writer claim is always free here.
        let writer = ring
            .writer(data, space)
            .expect("output ring is bound to exactly one writer at build");
        Output::new(writer)
    }
}

impl<F, WD, WS> Output<F, WD, WS>
where
    F: Frame + IntoBytes + Immutable,
    WD: WakeSource,
    WS: WakeSink,
{
    /// Publish a fixed frame (no dynamic members) as one record.
    pub fn write(&mut self, frame: &F) -> Result<(), metor_fsw_ring::WriteError> {
        self.writer.try_write(frame.as_bytes())
    }

    /// Publish a fixed frame, counting a failed write instead of returning
    /// it. See the module docs for the write versus publish tradeoff.
    pub fn publish(&mut self, frame: &F) {
        if self.writer.try_write(frame.as_bytes()).is_err() {
            self.dropped.bump();
        }
    }

    /// Publish a frame with dynamic members, counting a failed write instead
    /// of returning it.
    pub fn publish_with(&mut self, fixed: &F, build: impl FnOnce(&mut FrameWriter<F>)) {
        if self.write_with(fixed, build).is_err() {
            self.dropped.bump();
        }
    }

    /// Publish a frame with dynamic `FrameList`/`FrameMap` members. `build`
    /// drives a [`FrameWriter<F>`] to append the trailer, then the finished
    /// table bytes are written as one record.
    pub fn write_with(
        &mut self,
        fixed: &F,
        build: impl FnOnce(&mut FrameWriter<F>),
    ) -> Result<(), metor_fsw_ring::WriteError> {
        let scratch = self
            .scratch
            .take()
            .unwrap_or_else(|| FrameScratch::new(F::MAX_SIZE.min(1 << 16)));
        let mut fw = FrameWriter::from_scratch(scratch, fixed);
        build(&mut fw);
        let res = self.writer.try_write(fw.table());
        // Retain the (grown) backing for the next publish.
        self.scratch = Some(fw.finish());
        res
    }

    /// Publish a fixed frame, suspending until a reader frees room.
    pub async fn write_async(&mut self, frame: &F) -> Result<(), metor_fsw_ring::WriteError> {
        self.writer.write(frame.as_bytes()).await
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// An input consumes the frames an upstream [`Output`] publishes, through a
/// read-only [`View`] of that output's ring. Records are read in place and
/// handed out as a typed [`FrameGrant`] or [`FrameRef`]; the writer never
/// overwrites a record a reader has yet to release.
pub struct Input<F, RD = NoWake, RS = NoWake>
where
    RD: WakeSink,
    RS: WakeSource,
{
    view: View<RD, RS>,
    _f: PhantomData<F>,
}

impl<F: Frame, RD: WakeSink, RS: WakeSource> Input<F, RD, RS> {
    /// Wrap a view registered on the producing buffer.
    pub fn new(view: View<RD, RS>) -> Self {
        Self {
            view,
            _f: PhantomData,
        }
    }

    /// This port's static descriptor, the required producer shape.
    pub fn descriptor() -> PortDesc {
        PortDesc::of::<F>()
    }

    /// This field's entry in the bundle's [`decls`](crate::SystemInput::decls)
    /// walk, an ordinary wired port.
    pub fn decl() -> PortDecl {
        PortDecl::Port(Self::descriptor())
    }
}

impl<F: Frame, RD, RS> Input<F, RD, RS>
where
    RD: WakeSink + Default + Clone + 'static,
    RS: WakeSource + Default + Clone + 'static,
{
    /// Bind this input over the next ring the [`RingSource`] hands out,
    /// taking the reader-side wake endpoints matched to that buffer. The
    /// reader slot was reserved at sizing time, so registering the view
    /// cannot fail.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        let (ring, data, space) = src.next_input::<RD, RS>();
        Input::new(
            ring.view(data, space)
                .expect("reader slot reserved at sizing time"),
        )
    }
}

impl<F, RD, RS> Input<F, RD, RS>
where
    F: Frame + FromBytes + KnownLayout + Immutable,
    RD: WakeSink,
    RS: WakeSource,
{
    /// The newest committed record as a typed zero-copy borrow, or `None` if
    /// no record has ever arrived.
    ///
    /// Older unread records are consumed on the way, but the newest stays
    /// pinned on the ring with the view's cursor parked at its start. A later
    /// call with no new data is served the same record again, and the writer
    /// waits for space rather than overwrite it, which is why every ring
    /// holds at least two records. A corrupt region reads as `None`.
    pub fn latest(&mut self) -> Option<FrameGrant<'_, F, RS>> {
        self.view.try_latest().ok().flatten().map(FrameGrant::new)
    }

    /// Hand every record since the last drain to `f` in order, freeing each
    /// one for the writer as soon as `f` returns.
    pub fn drain(&mut self, mut f: impl FnMut(FrameRef<'_, F>)) -> Result<(), ReadError> {
        drain_view(&mut self.view, |rec| f(FrameRef::new(rec)))
    }

    /// Await the next record, suspending until one commits. The record is
    /// freed for the writer when the grant drops.
    pub async fn recv(&mut self) -> Result<FrameGrant<'_, F, RS>, ReadError> {
        Ok(FrameGrant::new(self.view.read().await?))
    }
}

// ---------------------------------------------------------------------------
// FrameRef / FrameGrant
// ---------------------------------------------------------------------------

/// A frame ref reads one record's table bytes in place, giving typed access
/// without copying. The fixed region is read directly as `F`, dynamic
/// members are read with [`ListReader`] and [`MapReader`], and
/// [`apply`](Self::apply) walks the whole record through the frame's vtable
/// for consumers that want components rather than the concrete type.
pub struct FrameRef<'a, F> {
    table: &'a [u8],
    _f: PhantomData<F>,
}

impl<'a, F> FrameRef<'a, F>
where
    F: Frame + FromBytes + KnownLayout + Immutable,
{
    fn new(table: &'a [u8]) -> Self {
        Self {
            table,
            _f: PhantomData,
        }
    }

    /// The fixed region, read in place. The table bytes at offset 0 are the
    /// `F` layout itself, so no per-field decode is needed.
    pub fn get(&self) -> &'a F {
        let (frame, _) =
            F::ref_from_prefix(self.table).expect("record shorter than F fixed region");
        frame
    }

    /// The raw table bytes, fixed region plus trailer.
    pub fn table(&self) -> &'a [u8] {
        self.table
    }

    /// A reader over the `FrameList<T, _>` member whose slot sits at
    /// `slot_off` (use `core::mem::offset_of!`).
    pub fn list<T: FromBytes>(&self, slot_off: usize) -> ListReader<'a, T> {
        ListReader::new(self.table, self.slot(slot_off))
    }

    /// A reader over the `FrameMap<_, V, _>` member whose slot sits at
    /// `slot_off`.
    pub fn map<V: FromBytes>(&self, slot_off: usize) -> MapReader<'a, V> {
        MapReader::new(self.table, self.slot(slot_off))
    }

    /// Drive a [`Decomponentize`] sink from this record via the frame's
    /// vtable.
    pub fn apply<D: Decomponentize>(
        &self,
        sink: &mut D,
    ) -> Result<Result<(), D::Error>, ProtoError> {
        F::as_vtable().apply(self.table, sink)
    }

    /// Read the 8-byte dynamic-member slot at `slot_off` from the fixed
    /// region.
    fn slot(&self, slot_off: usize) -> Slot {
        self.table
            .get(slot_off..)
            .and_then(|b| Slot::read_from_prefix(b).ok())
            .map(|(s, _)| s)
            .unwrap_or_default()
    }
}

/// A frame grant holds one record on the ring while the caller reads it,
/// pairing the ring [`ReadGrant`] that keeps the record alive with the
/// typed accessors of [`FrameRef`]. Returned by the reads that hand back a
/// record ([`Input::latest`], [`Input::recv`]); a callback drain passes a
/// plain [`FrameRef`] instead. Dropping it releases the record per the
/// grant's semantics, consumed for `recv` and kept pinned for `latest`.
pub struct FrameGrant<'a, F, RS = NoWake>
where
    RS: WakeSource,
{
    grant: ReadGrant<'a, RS>,
    _f: PhantomData<F>,
}

impl<'a, F, RS> FrameGrant<'a, F, RS>
where
    F: Frame + FromBytes + KnownLayout + Immutable,
    RS: WakeSource,
{
    pub(crate) fn new(grant: ReadGrant<'a, RS>) -> Self {
        Self {
            grant,
            _f: PhantomData,
        }
    }

    /// The borrowed record as a [`FrameRef`].
    pub fn as_ref(&self) -> FrameRef<'_, F> {
        FrameRef::new(&self.grant)
    }

    /// The fixed region, read in place (see [`FrameRef::get`]).
    pub fn get(&self) -> &F {
        self.as_ref().get()
    }

    /// The raw table bytes, fixed region plus trailer.
    pub fn table(&self) -> &[u8] {
        &self.grant
    }

    /// A reader over the `FrameList<T, _>` member whose slot sits at
    /// `slot_off`.
    pub fn list<T: FromBytes>(&self, slot_off: usize) -> ListReader<'_, T> {
        self.as_ref().list(slot_off)
    }

    /// A reader over the `FrameMap<_, V, _>` member whose slot sits at
    /// `slot_off`.
    pub fn map<V: FromBytes>(&self, slot_off: usize) -> MapReader<'_, V> {
        self.as_ref().map(slot_off)
    }

    /// Drive a [`Decomponentize`] sink from this record (see
    /// [`FrameRef::apply`]).
    pub fn apply<D: Decomponentize>(
        &self,
        sink: &mut D,
    ) -> Result<Result<(), D::Error>, ProtoError> {
        self.as_ref().apply(sink)
    }
}

/// A grant reads as the frame itself, so field access needs no `.get()`:
/// `input.latest()` then `record.omega`. The borrow is tied to the grant
/// (not the record's full ring lifetime), which is what keeps the deref'd
/// reference from outliving the pin.
impl<F, RS> core::ops::Deref for FrameGrant<'_, F, RS>
where
    F: Frame + FromBytes + KnownLayout + Immutable,
    RS: WakeSource,
{
    type Target = F;

    fn deref(&self) -> &F {
        self.get()
    }
}
