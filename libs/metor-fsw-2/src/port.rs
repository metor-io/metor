//! Typed port wrappers binding a [`Frame`] to one ring handle (system.md §2).
//!
//! [`Output<F>`] wraps the single ring [`Writer`] a system owns for frame `F`;
//! [`Input<F>`] wraps a read-only [`View`]. Both are thin: the data path (table
//! bytes == ring payload) is entirely the `FrameWriter`/`View` machinery —
//! these add only the frame typing, the latest-wins / every-record drains, and the
//! zero-copy fixed-region accessor. Reads are zero-copy borrows straight off the
//! ring (the lossless writer can never overwrite an unread record), handed out as
//! a typed [`FrameRef`] / [`FrameGrant`].

use core::marker::PhantomData;

use metor_fsw::Decomponentize;
use metor_fsw_ring::{
    NoWake, ReadError, ReadGrant, View, WakeSink, WakeSource, Writer, frame_len,
};
use metor_proto::error::Error as ProtoError;
use metor_proto::types::LenPacket;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::binder::RingSource;
use crate::descriptor::{PortDecl, PortDesc};
use crate::dynamic::Slot;
use crate::frame::Frame;
use crate::reader::{ListReader, MapReader};
use crate::writer::FrameWriter;

/// Default in-flight record depth for every port (system.md §2.2 / Q10). At least 2
/// (one in-flight while the slowest reader holds one — a latest-wins consumer
/// permanently pins the newest record, see [`Input::latest`]).
pub const DEFAULT_DEPTH: usize = 8;

/// Power-of-two ring capacity for `depth` records of a frame with `max_size`
/// worst-case table bytes (system.md §2.2). `frame_len` adds the 8-byte record
/// header + 8-byte payload padding.
pub fn capacity_for(max_size: usize, depth: usize) -> usize {
    (frame_len(max_size) * depth.max(2)).next_power_of_two()
}

/// As [`capacity_for`] but reading the worst-case size straight from the frame type.
pub fn buffer_capacity<F: Frame>(depth: usize) -> usize {
    capacity_for(F::MAX_SIZE, depth)
}

/// THE shared drain loop (C4): hand `f` a zero-copy borrow of every committed
/// record since the last drain, in order. Each grant drops (freeing the record
/// for the writer) before the next is taken. [`ReadError::Corrupt`] — a
/// structurally invalid region, unreachable from in-crate behavior — stops the
/// drain and propagates.
///
/// Used by [`Input::drain`], [`MsgIn::drain`](crate::MsgIn), and the telemetry
/// tap's log lane.
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

/// One owned output: the single [`Writer`] into a ring carrying frame `F`. Cyclic
/// outputs default to [`NoWake`]; async outputs select a `Notifier` wake so a
/// write can suspend for space.
pub struct Output<F, WD = NoWake, WS = NoWake>
where
    WD: WakeSource,
    WS: WakeSink,
{
    writer: Writer<WD, WS>,
    /// A reused table-bytes buffer for the dynamic-member [`write_with`](Self::write_with)
    /// path, so a per-cycle publish (health, status, dynamic frames) does not
    /// malloc+free a fresh `LenPacket` every call. `None` until the first such write.
    scratch: Option<LenPacket>,
    /// Records dropped by the infallible [`publish`](Self::publish) path (review E6):
    /// a write failure is `InsufficientCapacity` (a sizing bug) or `WouldBlock`
    /// (a slow reader backpressuring this ring) — either way the port counts it
    /// here and the runner folds the count into health via
    /// [`take_dropped`](Self::take_dropped).
    dropped: u64,
    _f: PhantomData<F>,
}

impl<F: Frame, WD: WakeSource, WS: WakeSink> Output<F, WD, WS> {
    /// Wrap a writer the coordinator created over an `F`-sized buffer.
    pub fn new(writer: Writer<WD, WS>) -> Self {
        Self {
            writer,
            scratch: None,
            dropped: 0,
            _f: PhantomData,
        }
    }

    /// This port's static descriptor (for wiring/sizing before any data flows).
    pub fn descriptor() -> PortDesc {
        PortDesc::of::<F>()
    }

    /// What this field contributes to the bundle's [`decls`](crate::SystemOutput::decls)
    /// walk: an ordinary wired port.
    pub fn decl() -> PortDecl {
        PortDecl::Port(Self::descriptor())
    }

    /// Take (sum-and-clear) the [`publish`](Self::publish) drop counter. Called by
    /// the derived `SystemOutput::take_dropped`, whose sum the runner telemeteres as
    /// a `publish_dropped` health error (review E6).
    pub fn take_dropped(&mut self) -> u64 {
        core::mem::take(&mut self.dropped)
    }
}

impl<F: Frame, WD, WS> Output<F, WD, WS>
where
    WD: WakeSource + Default + Clone + 'static,
    WS: WakeSink + Default + Clone + 'static,
{
    /// Bind this output over the next ring the [`RingSource`] hands out, taking the
    /// matched writer-side wake endpoints for that buffer (coordinator.md §1.4).
    /// Rings are backing-erased, so a dlopen'd system binds over the host's raw
    /// regions with this same monomorphic code path. Walked in `descriptors()`
    /// order by the generated bundle `bind`.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        let (ring, data, space) = src.next_output::<WD, WS>();
        // Invariant: the coordinator allocates one ring per output port and binds
        // it exactly once, so the region's writer claim is always free here.
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
    /// Publish a *fixed* frame (no dynamic members). The frame's `#[repr(C)]` bytes
    /// **are** its table bytes (offset 0 at the fixed region), so this is a single
    /// `try_write` with no serialization step (system.md §2.1).
    pub fn write(&mut self, frame: &F) -> Result<(), metor_fsw_ring::WriteError> {
        self.writer.try_write(frame.as_bytes())
    }

    /// Publish a fixed frame, **infallibly** (review E6): a failed write —
    /// `InsufficientCapacity` (a sizing bug) or `WouldBlock` (a slow reader
    /// backpressuring the ring; the record is dropped rather than blocking the
    /// cycle) — is counted for the runner to fold into health (`publish_dropped`)
    /// instead of returned. Sizing-aware callers keep [`write`](Self::write).
    pub fn publish(&mut self, frame: &F) {
        if self.writer.try_write(frame.as_bytes()).is_err() {
            self.dropped += 1;
        }
    }

    /// The [`write_with`](Self::write_with) twin of [`publish`](Self::publish):
    /// publish a frame with dynamic members, counting (not returning) a failure.
    pub fn publish_with(&mut self, fixed: &F, build: impl FnOnce(&mut FrameWriter<F>)) {
        if self.write_with(fixed, build).is_err() {
            self.dropped += 1;
        }
    }

    /// Publish a frame with dynamic `FrameList`/`FrameMap` members: `build` drives a
    /// [`FrameWriter<F>`] (its `list`/`map` builders) to append the trailer, then the
    /// finished table bytes are written as one record.
    pub fn write_with(
        &mut self,
        fixed: &F,
        build: impl FnOnce(&mut FrameWriter<F>),
    ) -> Result<(), metor_fsw_ring::WriteError> {
        let packet = self
            .scratch
            .take()
            .unwrap_or_else(|| LenPacket::table([0, 0], F::MAX_SIZE.min(1 << 16)));
        let mut fw = FrameWriter::from_packet(packet, fixed);
        build(&mut fw);
        let res = self.writer.try_write(fw.table());
        // Retain the (grown) buffer for the next publish.
        self.scratch = Some(fw.finish());
        res
    }

    /// Async publish of a fixed frame: suspends until a reader frees room. The
    /// async output path async systems use (system.md §3.2).
    pub async fn write_async(&mut self, frame: &F) -> Result<(), metor_fsw_ring::WriteError> {
        self.writer.write(frame.as_bytes()).await
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// One borrowed input: a [`View`] into an upstream output buffer (cyclic) or a
/// private copy-in buffer (async). Reads are zero-copy: the ring hands out a
/// borrow of the record in place (the writer can never overwrite an unread
/// record), wrapped as a typed [`FrameGrant`] / [`FrameRef`].
pub struct Input<F, RD = NoWake, RS = NoWake>
where
    RD: WakeSink,
    RS: WakeSource,
{
    view: View<RD, RS>,
    _f: PhantomData<F>,
}

impl<F: Frame, RD: WakeSink, RS: WakeSource> Input<F, RD, RS> {
    /// Wrap a view the coordinator registered on the producing buffer.
    pub fn new(view: View<RD, RS>) -> Self {
        Self {
            view,
            _f: PhantomData,
        }
    }

    /// This port's static descriptor (the required producer shape).
    pub fn descriptor() -> PortDesc {
        PortDesc::of::<F>()
    }

    /// What this field contributes to the bundle's [`decls`](crate::SystemInput::decls)
    /// walk: an ordinary wired port.
    pub fn decl() -> PortDecl {
        PortDecl::Port(Self::descriptor())
    }
}

impl<F: Frame, RD, RS> Input<F, RD, RS>
where
    RD: WakeSink + Default + Clone + 'static,
    RS: WakeSource + Default + Clone + 'static,
{
    /// Bind this input over the next ring the [`RingSource`] hands out, taking the
    /// matched reader-side wake endpoints (coordinator.md §1.4). The reader slot was
    /// reserved at sizing time, so registering the view cannot fail.
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
    /// The newest committed record as a typed zero-copy borrow, or `None` if no
    /// record has ever arrived. Cyclic systems want the freshest sample, not a
    /// backlog (system.md §2.3).
    ///
    /// Older unread records are consumed (freed for the writer) on the way; the
    /// newest stays **pinned** on the ring — the view's cursor parks at its start —
    /// so a later cycle with no new data is served the same record again, and the
    /// writer backpressures rather than overwrite it (`DEFAULT_DEPTH` absorbs the
    /// one pinned record). A corrupt region (unreachable from in-crate behavior)
    /// reads as `None`.
    pub fn latest(&mut self) -> Option<FrameGrant<'_, F, RS>> {
        self.view.try_latest().ok().flatten().map(FrameGrant::new)
    }

    /// Process **every** record since the last drain, in order (for command / event
    /// channels that cannot drop a record). Each record is handed to `f` as a
    /// zero-copy [`FrameRef`] and freed for the writer as soon as `f` returns.
    pub fn drain(&mut self, mut f: impl FnMut(FrameRef<'_, F>)) -> Result<(), ReadError> {
        drain_view(&mut self.view, |rec| f(FrameRef::new(rec)))
    }

    /// Await the next record (event-driven async systems, system.md §3.2). Backed by
    /// the view's async `read`, which suspends on the `RD` wake until data commits.
    /// The record is consumed (freed for the writer) when the grant drops.
    pub async fn recv(&mut self) -> Result<FrameGrant<'_, F, RS>, ReadError> {
        Ok(FrameGrant::new(self.view.read().await?))
    }
}

// ---------------------------------------------------------------------------
// FrameRef / FrameGrant — typed access over one record's table bytes
// ---------------------------------------------------------------------------

/// A typed, zero-copy view of one record's table bytes (system.md §2.3): the fixed
/// region is read directly as `F`; dynamic members are read with the
/// `ListReader`/`MapReader`; the vtable `apply` is the uniform escape hatch.
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

    /// The fixed `#[repr(C)]` region, zero-copy. The table bytes at offset 0 *are*
    /// the `F` layout (the producer wrote `fixed.as_bytes()` there), so no per-field
    /// decode is needed.
    pub fn get(&self) -> &'a F {
        let (frame, _) = F::ref_from_prefix(self.table).expect("record shorter than F fixed region");
        frame
    }

    /// The raw table bytes (fixed region + trailer).
    pub fn table(&self) -> &'a [u8] {
        self.table
    }

    /// A reader over the `FrameList<T, _>` member whose slot sits at `slot_off`
    /// (use `core::mem::offset_of!`).
    pub fn list<T: FromBytes>(&self, slot_off: usize) -> ListReader<'a, T> {
        ListReader::new(self.table, self.slot(slot_off))
    }

    /// A reader over the `FrameMap<_, V, _>` member whose slot sits at `slot_off`.
    pub fn map<V: FromBytes>(&self, slot_off: usize) -> MapReader<'a, V> {
        MapReader::new(self.table, self.slot(slot_off))
    }

    /// Drive any [`Decomponentize`] sink from this record via the frame's vtable —
    /// the same path metor-db uses (system.md §2.3, the escape hatch).
    pub fn apply<D: Decomponentize>(&self, sink: &mut D) -> Result<Result<(), D::Error>, ProtoError> {
        F::as_vtable().apply(self.table, sink)
    }

    /// Read the 8-byte dynamic-member slot at `slot_off` from the fixed region.
    fn slot(&self, slot_off: usize) -> Slot {
        self.table
            .get(slot_off..)
            .and_then(|b| Slot::read_from_prefix(b).ok())
            .map(|(s, _)| s)
            .unwrap_or_default()
    }
}

/// An owning typed read guard: a ring [`ReadGrant`] (the zero-copy borrow of one
/// record, holding the view's cursor) plus the [`FrameRef`] accessor surface.
/// Returned by the reads that *hand back* a record ([`Input::latest`],
/// [`Input::recv`]) — a callback drain passes a plain [`FrameRef`] instead.
/// Dropping it releases the record per the grant's semantics (consume for
/// `recv`, keep-pinned for `latest`).
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

    /// The borrowed record as a [`FrameRef`] (for passing to `FrameRef`-taking code).
    pub fn as_ref(&self) -> FrameRef<'_, F> {
        FrameRef::new(&self.grant)
    }

    /// The fixed `#[repr(C)]` region, zero-copy (see [`FrameRef::get`]).
    pub fn get(&self) -> &F {
        self.as_ref().get()
    }

    /// The raw table bytes (fixed region + trailer).
    pub fn table(&self) -> &[u8] {
        &self.grant
    }

    /// A reader over the `FrameList<T, _>` member whose slot sits at `slot_off`.
    pub fn list<T: FromBytes>(&self, slot_off: usize) -> ListReader<'_, T> {
        self.as_ref().list(slot_off)
    }

    /// A reader over the `FrameMap<_, V, _>` member whose slot sits at `slot_off`.
    pub fn map<V: FromBytes>(&self, slot_off: usize) -> MapReader<'_, V> {
        self.as_ref().map(slot_off)
    }

    /// Drive any [`Decomponentize`] sink from this record (see [`FrameRef::apply`]).
    pub fn apply<D: Decomponentize>(&self, sink: &mut D) -> Result<Result<(), D::Error>, ProtoError> {
        self.as_ref().apply(sink)
    }
}
