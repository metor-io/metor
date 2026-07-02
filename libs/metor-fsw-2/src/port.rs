//! Typed port wrappers binding a [`Frame`] to one ring handle (system.md §2).
//!
//! [`Output<F>`] wraps the single ring [`Writer`] a system owns for frame `F`;
//! [`Input<F>`] wraps a read-only [`View`]. Both are thin: the data path (table
//! bytes == ring payload) is entirely the `FrameWriter`/`View` machinery —
//! these add only the frame typing, the latest-wins / every-record drains, and the
//! zero-copy fixed-region accessor.

use core::marker::PhantomData;

use metor_fsw::Decomponentize;
use metor_fsw_ring::{
    Backing, BoxBacking, NoWake, ReadError, View, WakeSink, WakeSource, Writer, frame_len,
};
use metor_proto::error::Error as ProtoError;
use metor_proto::types::LenPacket;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::binder::RingSource;
use crate::descriptor::PortDesc;
use crate::dynamic::Slot;
use crate::frame::Frame;
use crate::reader::{ListReader, MapReader};
use crate::writer::FrameWriter;

/// Default in-flight record depth for every port (system.md §2.2 / Q10). At least 2
/// (one in-flight while the slowest reader holds one).
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

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// One owned output: the single [`Writer`] into a ring carrying frame `F`. Cyclic
/// outputs default to [`BoxBacking`] + [`NoWake`]; async outputs select a
/// `Notifier` wake so a lossless write can suspend for space.
pub struct Output<F, B = BoxBacking, WD = NoWake, WS = NoWake>
where
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    writer: Writer<B, WD, WS>,
    /// A reused table-bytes buffer for the dynamic-member [`write_with`](Self::write_with)
    /// path, so a per-cycle publish (health, status, dynamic frames) does not
    /// malloc+free a fresh `LenPacket` every call. `None` until the first such write.
    scratch: Option<LenPacket>,
    _f: PhantomData<F>,
}

impl<F: Frame, B: Backing, WD: WakeSource, WS: WakeSink> Output<F, B, WD, WS> {
    /// Wrap a writer the coordinator created over an `F`-sized buffer.
    pub fn new(writer: Writer<B, WD, WS>) -> Self {
        Self {
            writer,
            scratch: None,
            _f: PhantomData,
        }
    }

    /// This port's static descriptor (for wiring/sizing before any data flows).
    pub fn descriptor() -> PortDesc {
        PortDesc::of::<F>()
    }
}

impl<F: Frame, B: Backing, WD, WS> Output<F, B, WD, WS>
where
    WD: WakeSource + Default + Clone + 'static,
    WS: WakeSink + Default + Clone + 'static,
{
    /// Bind this output over the next ring the [`RingSource`] hands out, taking the
    /// matched writer-side wake endpoints for that buffer (coordinator.md §1.4). The
    /// source's backing `B` is this port's backing, so a dlopen'd system binds
    /// `Output<F, RawBacking>` over the host's regions with the same code path.
    /// Walked in `descriptors()` order by the generated bundle `bind`.
    pub fn bind<S: RingSource<B = B>>(src: &mut S) -> Self {
        let (ring, data, space) = src.next_output::<WD, WS>();
        Output::new(ring.writer(data, space))
    }
}

impl<F, B, WD, WS> Output<F, B, WD, WS>
where
    F: Frame + IntoBytes + Immutable,
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    /// Publish a *fixed* frame (no dynamic members). The frame's `#[repr(C)]` bytes
    /// **are** its table bytes (offset 0 at the fixed region), so this is a single
    /// `try_write` with no serialization step (system.md §2.1).
    pub fn write(&mut self, frame: &F) -> Result<(), metor_fsw_ring::WriteError> {
        self.writer.try_write(frame.as_bytes())
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

    /// Async publish of a fixed frame: suspends (lossless mode only) until there is
    /// room. The async output path async systems use (system.md §3.2).
    pub async fn write_async(&mut self, frame: &F) -> Result<(), metor_fsw_ring::WriteError> {
        self.writer.write(frame.as_bytes()).await
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// One borrowed input: a [`View`] into an upstream output buffer (cyclic) or a
/// private copy-in buffer (async). Reads copy the newest record(s) into a reused
/// `scratch` and hand them out as a zero-copy typed [`FrameRef`].
pub struct Input<F, B = BoxBacking, RD = NoWake, RS = NoWake>
where
    B: Backing,
    RD: WakeSink,
    RS: WakeSource,
{
    view: View<B, RD, RS>,
    scratch: Vec<u8>,
    /// Whether `scratch` holds a valid record (so `latest` can keep returning the
    /// freshest one across calls with no new data).
    have: bool,
    _f: PhantomData<F>,
}

impl<F: Frame, B: Backing, RD: WakeSink, RS: WakeSource> Input<F, B, RD, RS> {
    /// Wrap a view the coordinator registered on the producing buffer.
    pub fn new(view: View<B, RD, RS>) -> Self {
        Self {
            view,
            scratch: Vec::new(),
            have: false,
            _f: PhantomData,
        }
    }

    /// This port's static descriptor (the required producer shape).
    pub fn descriptor() -> PortDesc {
        PortDesc::of::<F>()
    }

    /// True iff the writer lapped this view (overwrite buffers only). The
    /// coordinator checks this on cyclic systems *before* `execute` (system.md §3.1);
    /// the stop policy itself lives in the coordinator.
    pub fn is_lapped(&self) -> bool {
        self.view.is_lapped()
    }

    /// Skip to the live edge, abandoning unread (possibly lapped) data. Async input
    /// ports call this on lap to "drop on full and continue" (system.md §3.2).
    pub fn resync(&self) {
        self.view.resync();
    }
}

impl<F: Frame, B: Backing, RD, RS> Input<F, B, RD, RS>
where
    RD: WakeSink + Default + Clone + 'static,
    RS: WakeSource + Default + Clone + 'static,
{
    /// Bind this input over the next ring the [`RingSource`] hands out, taking the
    /// matched reader-side wake endpoints (coordinator.md §1.4). The reader slot was
    /// reserved at sizing time, so registering the view cannot fail. The source's
    /// backing `B` is this port's backing.
    pub fn bind<S: RingSource<B = B>>(src: &mut S) -> Self {
        let (ring, data, space) = src.next_input::<RD, RS>();
        Input::new(
            ring.view(data, space)
                .expect("reader slot reserved at sizing time"),
        )
    }
}

impl<F, B, RD, RS> Input<F, B, RD, RS>
where
    F: Frame + FromBytes + KnownLayout + Immutable,
    B: Backing,
    RD: WakeSink,
    RS: WakeSource,
{
    /// Drain to the newest committed record and hand back a typed view of it, or
    /// `None` if no record has ever arrived. Cyclic systems want the freshest
    /// sample, not a backlog (system.md §2.3); a `Lapped` view is surfaced as
    /// `Err`.
    pub fn latest(&mut self) -> Result<Option<FrameRef<'_, F>>, ReadError> {
        loop {
            match self.view.try_read_into(&mut self.scratch) {
                Ok(true) => self.have = true,
                Ok(false) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(self.have.then(|| FrameRef::new(&self.scratch)))
    }

    /// Process **every** record since the last drain, in order (for command / event
    /// channels that cannot drop a record). Stops and returns `Err` on lap.
    pub fn drain(&mut self, mut f: impl FnMut(FrameRef<'_, F>)) -> Result<(), ReadError> {
        loop {
            match self.view.try_read_into(&mut self.scratch) {
                Ok(true) => {
                    self.have = true;
                    f(FrameRef::new(&self.scratch));
                }
                Ok(false) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    /// Await the next record (event-driven async systems, system.md §3.2). Backed by
    /// the view's async `read_into`, which suspends on the `RD` wake until data
    /// commits. Propagates `Lapped`.
    pub async fn recv(&mut self) -> Result<FrameRef<'_, F>, ReadError> {
        self.view.read_into(&mut self.scratch).await?;
        self.have = true;
        Ok(FrameRef::new(&self.scratch))
    }
}

// ---------------------------------------------------------------------------
// FrameRef — typed access over one record's table bytes
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
