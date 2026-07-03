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
use crate::descriptor::{OnLap, PortDecl, PortDesc};
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

/// THE shared drain loop (C4): drain `view` into `scratch`, calling `f` once per
/// committed record. On a lap, apply `on_lap` — [`OnLap::Resync`] skips to the live
/// edge and keeps draining; [`OnLap::Stop`] stops draining immediately. Returns
/// whether a lap was **observed** (raw, policy-free — the caller derives its
/// `lap_fault` from its own policy, so a Resync port can still latch the
/// observation for diagnostics).
///
/// Used by [`Input::latest`]/[`Input::drain`], [`MsgIn::drain`](crate::MsgIn), the
/// coordinator's copy-in jobs, and both telemetry tap lanes.
pub(crate) fn drain_view<B, RD, RS>(
    view: &mut View<B, RD, RS>,
    scratch: &mut Vec<u8>,
    on_lap: OnLap,
    mut f: impl FnMut(&[u8]),
) -> bool
where
    B: Backing,
    RD: WakeSink,
    RS: WakeSource,
{
    let mut lapped = false;
    loop {
        match view.try_read_into(scratch) {
            Ok(true) => f(scratch),
            Ok(false) => return lapped,
            Err(_) => {
                lapped = true;
                match on_lap {
                    OnLap::Resync => view.resync(),
                    OnLap::Stop => return lapped,
                }
            }
        }
    }
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
    /// Records dropped by the infallible [`publish`](Self::publish) path (review E6):
    /// on the framework's Overwrite rings a write failure is only ever
    /// `InsufficientCapacity` — a sizing bug — so the port counts it here and the
    /// runner folds the count into health via [`take_dropped`](Self::take_dropped).
    dropped: u64,
    _f: PhantomData<F>,
}

impl<F: Frame, B: Backing, WD: WakeSource, WS: WakeSink> Output<F, B, WD, WS> {
    /// Wrap a writer the coordinator created over an `F`-sized buffer.
    pub fn new(writer: Writer<B, WD, WS>) -> Self {
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
        // Invariant: the coordinator allocates one ring per output port and binds
        // it exactly once, so the region's writer claim is always free here.
        let writer = ring
            .writer(data, space)
            .expect("output ring is bound to exactly one writer at build");
        Output::new(writer)
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

    /// Publish a fixed frame, **infallibly** (review E6): the only failure on the
    /// framework's Overwrite rings is `InsufficientCapacity` (a sizing bug), so
    /// instead of a `Result` the drop is counted for the runner to fold into health
    /// (`publish_dropped`). Sizing-aware callers keep [`write`](Self::write).
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
    /// A lap this port observed — pre-read (`view.is_lapped()`) or mid-drain (the E3
    /// latch). Latched (never cleared) so the runner's post-execute
    /// [`lap_fault`](Self::lap_fault) check telemeteres a mid-execute lap the same
    /// cycle. Policy-free: whether the observation is a *fault* is the port's
    /// [`OnLap`] policy.
    lapped: bool,
    /// The lap policy (axis 4), default [`OnLap::Stop`] (the cyclic frame doctrine);
    /// set at bind from the field's descriptor attribute via
    /// [`with_on_lap`](Self::with_on_lap).
    on_lap: OnLap,
    _f: PhantomData<F>,
}

impl<F: Frame, B: Backing, RD: WakeSink, RS: WakeSource> Input<F, B, RD, RS> {
    /// Wrap a view the coordinator registered on the producing buffer.
    pub fn new(view: View<B, RD, RS>) -> Self {
        Self {
            view,
            scratch: Vec::new(),
            have: false,
            lapped: false,
            on_lap: OnLap::Stop,
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

    /// Override the runtime lap policy — chainable, mirroring the descriptor-level
    /// [`PortDesc::with_on_lap`]. The `#[fsw(on_lap = "…")]` derive attribute lowers
    /// onto both so descriptor and runtime can never disagree.
    pub fn with_on_lap(mut self, p: OnLap) -> Self {
        self.on_lap = p;
        self
    }

    /// Whether this port is in **lap fault**: a lap was observed (the writer
    /// overwrote unread data — before a read, or resynced-over mid-drain, the E3
    /// latch) *and* the port's policy says laps are fatal ([`OnLap::Stop`]). A
    /// Resync port reports `false` because its policy says laps are not faults —
    /// derived, not lied (A5). The runner checks this before and after `execute`;
    /// the stop policy itself lives in the coordinator.
    pub fn lap_fault(&self) -> bool {
        (self.lapped || self.view.is_lapped()) && self.on_lap == OnLap::Stop
    }

    /// Skip to the live edge, abandoning unread (possibly lapped) data. Async input
    /// ports call this on lap to "drop on full and continue" (system.md §3.2).
    /// `&mut self` for consistency with `latest`/`drain` (S5): every read-side
    /// cursor move spells the same way (the ring's own `View::resync` is interior).
    pub fn resync(&mut self) {
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
    /// sample, not a backlog (system.md §2.3).
    ///
    /// A lap mid-drain (an async producer racing this cycle — the coordinator
    /// already hard-stops a Stop port on a lap seen *before* `execute`) is
    /// **resync-latched** (review E3): the view skips to the live edge, the lap is
    /// remembered, and the freshest record keeps being served. The mechanics are
    /// policy-free on purpose — whether the latched lap is a *fault* is
    /// [`lap_fault`](Self::lap_fault)'s call: on a Stop port the runner's pre/post
    /// execute check hard-stops the consumer (so the resynced read only ever feeds
    /// the doomed cycle, exactly today's doctrine); on a Resync port — including
    /// the async copy-in coercion, where the port cannot know its consumer is
    /// async — data keeps flowing. Lossless/async callers that must see the error
    /// use [`recv`](Self::recv)/[`drain`](Self::drain).
    pub fn latest(&mut self) -> Option<FrameRef<'_, F>> {
        let Self {
            view,
            scratch,
            have,
            lapped,
            ..
        } = self;
        *lapped |= drain_view(view, scratch, OnLap::Resync, |_| *have = true);
        self.have.then(|| FrameRef::new(&self.scratch))
    }

    /// Process **every** record since the last drain, in order (for command / event
    /// channels that cannot drop a record). On a lap, follows the port's policy:
    /// a Stop port stops draining and returns `Err(Lapped)`; a Resync port skips to
    /// the live edge, keeps draining, and returns `Ok` (laps are not faults there —
    /// the latch still records the observation).
    pub fn drain(&mut self, mut f: impl FnMut(FrameRef<'_, F>)) -> Result<(), ReadError> {
        let Self {
            view,
            scratch,
            have,
            lapped,
            on_lap,
            ..
        } = self;
        let saw_lap = drain_view(view, scratch, *on_lap, |rec| {
            *have = true;
            f(FrameRef::new(rec));
        });
        *lapped |= saw_lap;
        if saw_lap && *on_lap == OnLap::Stop {
            Err(ReadError::Lapped)
        } else {
            Ok(())
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
