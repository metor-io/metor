//! The cyclic run loop.
//!
//! A [`CoordinatorBuilder`] collects systems and the edges between their ports,
//! then `build()`s them (the init pipeline lives in [`init`]) into a ready
//! [`Coordinator`]. [`Coordinator::run_for`] drives the lifecycle: spawn the
//! async systems, init everything behind a barrier, step the cyclic systems
//! once per cycle, run the async copy-in mirror, publish coordinator-level
//! health and a status frame, and tear it all down.
//!
//! Cyclic systems step in registration order, once per cycle; the build-time
//! passes ([`init`]) reject any wiring whose dataflow disagrees with that
//! order, so the loop here never has to reason about staleness. Async systems
//! run on their own tasks, off the cycle clock, and observe their snapshot
//! inputs through the post-step copy-in mirror.

use core::mem::offset_of;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::time::{Duration, Instant};

use metor_fsw_ring::{NoWake, Notifier, RingBuffer, View, WakeSource, Writer};
use metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_proto_wkt::{ReloadSequences, SequenceCommand, SequenceRegistry, WiringManifest};
use stellarator::sync::WaitQueue;
use stellarator::{JoinHandle, JoinHandleDropGuard};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::binder::BindPorts;
use crate::descriptor::{Hz, PortId, SystemDescriptor};
use crate::dynamic::FrameList;
use crate::health::{HealthPort, Level};
use crate::message::{MsgIn, MsgOut};
use crate::port::Output;
use crate::proc::session::SessionDir;
use crate::registry::Registry;
use crate::system::{AsyncSystem, CyclicSystem, Out, SystemOutput};
use crate::{DEFAULT_DEPTH, Frame};

mod bind;
mod error;
mod init;
mod slot;
mod status;
use init::InitGraph;
pub use error::WireError;
pub(crate) use slot::validate_slot_spec;
pub use slot::{
    AllowedOccupant, InitialOccupant, OccupantBacking, SlotConfigError, SlotStatus,
};
pub use status::{NAME_CAP, SlotState, StopReason, StoppedSystem, WorkerRunState, WorkerStatus};
pub(crate) use status::{CyclicSlot, pack_name};

/// The default [`CoordinatorConfig::reader_slack`].
const READER_SLACK: usize = 4;

/// How long a teardown gives async tasks to exit cooperatively before their
/// `drop_guard` cancels them.
const JOIN_TIMEOUT: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// Public configuration / addressing
// ---------------------------------------------------------------------------

/// Which clock drives the per-cycle timestamp, and whether the loop paces itself.
#[derive(Clone, Copy, Debug, Default)]
pub enum ClockMode {
    /// Wall-clock time. Each cycle's `now` is `Timestamp::now()`, and the loop
    /// sleeps out the remainder of each cycle to hold `cycle_rate`. The default.
    #[default]
    Wall,
    /// A simulated clock. Each cycle's `now` advances by `dt` from a start epoch
    /// and the loop never sleeps, so cycles run as fast as the host allows. `dt`
    /// is the logical step, which keeps a mission converging in fixed simulated
    /// time no matter how fast it actually runs.
    Simulated { dt: Duration },
}

/// The settings a whole graph is built under. They fix the loop's pace, the
/// depth of buffers without a rate hint, the [`ClockMode`] stamping each
/// cycle, and the spare reader slots every ring keeps.
#[derive(Clone, Copy, Debug)]
pub struct CoordinatorConfig {
    /// The single global cycle rate the loop holds under a
    /// [`Wall`](ClockMode::Wall) clock. Every cyclic system runs every cycle;
    /// there is no per-system rate division. Ignored under a `Simulated` clock.
    pub cycle_rate: Hz,
    /// In-flight record depth for a buffer whose `PortDesc` carries no rate hint.
    pub default_depth: usize,
    /// The clock driving the per-cycle `now` and loop pacing (default `Wall`).
    pub clock: ClockMode,
    /// Spare reader slots added on top of every buffer's counted fan-out, for
    /// taps claimed through the [`Registry`](crate::Registry) after `build()`
    /// (a recorder, a debugger). Each buffer's `max_readers` is fixed at build
    /// time; exhausting the budget is a
    /// [`FullReaderTable`](metor_fsw_ring::FullReaderTable) error at the claim
    /// site. Default `4`.
    pub reader_slack: usize,
    /// How long a process system's step waits for the worker's ack before the
    /// cycle moves on. A lapse with the child alive is telemetered as a
    /// `proc_step_timeout` coordinator-health error; with the child dead it
    /// stops the slot ([`StopReason::ProcessDied`]) and, budget permitting,
    /// begins a restart. A healthy worker never approaches this — the wall
    /// cycle budget is usually far tighter. Default 100 ms.
    pub proc_step_timeout: Duration,
    /// How many times a process system's worker is respawned after it dies or
    /// its system panics, over the slot's whole life. Each restart is
    /// telemetered (`proc_restart` on coordinator health, and the worker list
    /// in the status frame); past the budget the stop is permanent, exactly
    /// like an in-process panic. `0` disables restart. Default 3.
    pub proc_max_restarts: u32,
    /// How long a dead worker's slot waits before respawning, so a
    /// crash-looping artifact cannot busy-spin the spawn path. Default 500 ms.
    pub proc_restart_backoff: Duration,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            cycle_rate: 100.0,
            default_depth: DEFAULT_DEPTH,
            clock: ClockMode::Wall,
            reader_slack: READER_SLACK,
            proc_step_timeout: Duration::from_millis(100),
            proc_max_restarts: 3,
            proc_restart_backoff: Duration::from_millis(500),
        }
    }
}

/// An opaque index naming one registered system. The builder's `add_*`
/// methods return it, and a [`PortRef`] embeds it to address that system's
/// ports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SystemHandle {
    id: usize,
}

/// A `(system, port)` pair addressing one port for wiring, both halves taken
/// from the system's registered [`SystemDescriptor`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PortRef {
    pub system: SystemHandle,
    pub port: PortId,
}

impl PortRef {
    /// Address the port carrying frame `F` on `system`.
    pub fn new<F: Frame>(system: SystemHandle) -> Self {
        Self {
            system,
            port: PortId::Component(F::FRAME_ID),
        }
    }

    /// Address the port carrying message `M` on `system`.
    pub fn msg<M: Msg>(system: SystemHandle) -> Self {
        Self {
            system,
            port: PortId::Packet(M::ID),
        }
    }
}

// ---------------------------------------------------------------------------
// Coordinator status frame
// ---------------------------------------------------------------------------

/// Max stopped systems named in one status record.
pub const MAX_STOPPED: usize = 32;

/// One stopped-system entry in [`CoordinatorStatus`]: a reason code, a used
/// name length, and a fixed-size name buffer.
#[derive(crate::AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
struct StoppedEntry {
    reason: u8,
    len: u8,
    _pad: [u8; 6],
    name: [u8; NAME_CAP],
}

/// Max process workers named in one status record.
pub const MAX_WORKERS: usize = 32;

/// One process system's worker entry in [`CoordinatorStatus`]: pid, restart
/// count, a [`WorkerRunState`] code, and the instance name. This is where
/// telemetry says a system runs out-of-process at all.
#[derive(crate::AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
struct WorkerEntry {
    /// The live worker's pid, or `0` between workers.
    pid: u32,
    restarts: u32,
    /// A [`WorkerRunState::code`].
    state: u8,
    len: u8,
    _pad: [u8; 6],
    name: [u8; NAME_CAP],
}

/// The coordinator's own status frame: which cyclic systems have hard-stopped
/// and why, plus the worker behind every process system (pid, restarts, run
/// state), in addition to each system's own health.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "coordinator_status")]
struct CoordinatorStatus {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    cycle: u64,
    stopped_count: u64,
    worker_count: u64,
    stopped: FrameList<StoppedEntry, MAX_STOPPED>,
    workers: FrameList<WorkerEntry, MAX_WORKERS>,
}

// ---------------------------------------------------------------------------
// Ring registry
// ---------------------------------------------------------------------------

/// The place a ring occupies in the graph, either a system's declared output
/// buffer or a dedicated input-side ring. The variant payloads are debug-only
/// (rendered via `Debug`, read by nothing), which is what the `allow` covers;
/// the variants themselves are matched (`output_instances`).
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
enum BufferRole {
    /// A system's declared output buffer, the coordinator's own #0 outputs
    /// included.
    Output { system: usize, port: usize },
    /// A dedicated input-side ring: an async copy-in buffer, or a
    /// host-connected input's ring.
    Private { system: usize, input: usize },
}

/// One owned ring plus its identity. `ring` is the canonical handle whose sole
/// job is to outlive every port over it (the ports hold their own `Arc` clones).
struct RingEntry {
    /// Held for ownership only (the ports clone their own `Arc`s), never read.
    #[allow(dead_code)]
    ring: RingBuffer,
    frame_id: ComponentId,
    role: BufferRole,
    /// The instance name of the system owning this buffer, which prefixes its
    /// telemetry key. `None` for coordinator-owned buffers.
    instance: Option<String>,
}

/// Owns every `RingBuffer`. Holding the canonical handle here keeps a buffer
/// alive longer than any port over it, regardless of teardown order.
struct RingTable {
    rings: Vec<RingEntry>,
}

// ---------------------------------------------------------------------------
// Async plumbing
// ---------------------------------------------------------------------------

/// One mirroring job that copies the newest upstream record into an async
/// system's private buffer, at most once per new upstream commit. It exists
/// only for snapshot inputs; the record is borrowed in place off the upstream
/// ring and written through, with no intermediate buffer.
struct CopyIn {
    upstream: View<NoWake>,
    /// The private ring's sole writer. The matched data `Notifier` wakes the
    /// parked async `recv`; a full private ring (the consumer is behind) drops
    /// this cycle's mirror rather than suspending the cycle loop.
    writer: Writer<Notifier>,
    /// The upstream ring's `committed` at the last mirror, so an unchanged
    /// upstream is skipped instead of re-waking the consumer with the same
    /// pinned record every cycle. `u64::MAX` means nothing mirrored yet.
    last_committed: u64,
}

/// Per-task signals the coordinator hands a spawned async system: a stop flag,
/// an init-readiness barrier, and a go-gate that holds the first `run` pass
/// until every system's `init` has completed.
struct LaunchCtx {
    stop: Arc<AtomicBool>,
    ready: Arc<WaitQueue>,
    ready_count: Arc<AtomicUsize>,
    go: Arc<WaitQueue>,
    go_flag: Arc<AtomicBool>,
}

/// Spawns a bound async system onto its own task, exactly once. Erased so the
/// coordinator can hold a heterogeneous set.
trait AsyncLauncher {
    fn launch(self: Box<Self>, ctx: LaunchCtx) -> JoinHandle<()>;
}

/// An async system packaged with its bound input and output ports. Its `run`
/// future borrows all three for the loop, so they move into the spawned task
/// together.
struct AsyncSlot<S: AsyncSystem> {
    system: S,
    input: S::Input,
    output: S::Output,
}

impl<S> AsyncLauncher for AsyncSlot<S>
where
    S: AsyncSystem + 'static,
    S::Input: 'static,
    S::Output: 'static,
{
    fn launch(self: Box<Self>, ctx: LaunchCtx) -> JoinHandle<()> {
        let mut me = *self;
        stellarator::spawn(async move {
            // Init inside the task (the only owner of the bundle), then signal
            // readiness and hold at the go-gate until every system's init is done.
            me.system.init(&mut me.output);
            ctx.ready_count.fetch_add(1, Release);
            ctx.ready.wake_all();
            let _ = ctx.go.wait_for(|| ctx.go_flag.load(Acquire)).await;
            loop {
                if ctx.stop.load(Acquire) {
                    break;
                }
                me.system.run(&mut me.input, &mut me.output).await;
            }
            me.system.shutdown(&mut me.output);
        })
    }
}

/// A spawned async task plus the handles the coordinator drives its lifecycle
/// with. The `drop_guard` cancels the task if it does not exit cooperatively
/// (and when a `Coordinator` is dropped mid-run).
struct AsyncTask {
    /// Held for its `Drop`: cancels the task on teardown (or coordinator drop).
    #[allow(dead_code)]
    handle: JoinHandleDropGuard<()>,
    stop: Arc<AtomicBool>,
    /// The input data-notifiers to wake so a task parked in `recv` re-polls.
    wake_on_stop: Vec<Notifier>,
}

/// A bound async system awaiting `run` (built at `build`, spawned at `run`).
struct PendingAsync {
    launcher: Box<dyn AsyncLauncher>,
    wake_on_stop: Vec<Notifier>,
}

// ---------------------------------------------------------------------------
// Builder (a thin shim over the init graph)
// ---------------------------------------------------------------------------

/// Registers systems and edges, then `build`s a ready [`Coordinator`]. A thin
/// façade over an [`InitGraph`]: every method records into the graph, and
/// `build` hands it to [`init::build`].
pub struct CoordinatorBuilder {
    graph: InitGraph,
}

impl CoordinatorBuilder {
    fn new(config: CoordinatorConfig) -> Self {
        Self {
            graph: InitGraph::new(config),
        }
    }

    /// The handle addressing the coordinator's own system-#0 bundle, so a
    /// front-end can wire the operator command edge with
    /// `connect(PortRef::msg::<SequenceCommand>(b.coordinator_handle()), …)`.
    pub fn coordinator_handle(&self) -> SystemHandle {
        self.graph.coordinator_handle()
    }

    /// Broadcast `manifest` as a [`WiringManifest`] at startup and on reload.
    ///
    /// The front-end ([`resolve`](crate::wiring::resolve)) hands over the full,
    /// path-stripped mission IR here; a `wiring` output is added to the
    /// coordinator #0 bundle, sized from the concrete JSON payload (which for a
    /// non-trivial mission exceeds `MAX_MSG_BYTES`), and the run loop emits it
    /// on the telemetry plane — the pattern [`SequenceRegistry`] uses. Called
    /// again, the latest manifest wins.
    pub fn set_wiring_manifest(&mut self, manifest: WiringManifest) {
        self.graph.set_wiring_manifest(manifest);
    }

    /// The registered descriptor of `handle`, which is what `build()`
    /// validates, sizes, and wires. For a slot this is the derived contract
    /// (see [`add_slot`](Self::add_slot)), which a front-end reads back
    /// instead of re-deriving; for everything else it is the system's own
    /// `descriptor()`.
    pub fn descriptor_of(&self, handle: SystemHandle) -> &SystemDescriptor {
        self.graph.descriptor_of(handle)
    }

    /// Register a cyclic system under its type's `System::NAME` instance name;
    /// see [`add_cyclic_named`](Self::add_cyclic_named) to name the instance
    /// explicitly.
    pub fn add_cyclic<S, O>(&mut self, system: S) -> SystemHandle
    where
        S: CyclicSystem<Output = Out<O>> + 'static,
        O: SystemOutput + BindPorts + 'static,
        S::Input: BindPorts + 'static,
    {
        self.graph.add_cyclic(system)
    }

    /// Register a cyclic system under an explicit instance name; returns a
    /// handle whose ports can be `connect`ed. The instance name disambiguates
    /// two instances of one system type in the telemetry keyspace.
    pub fn add_cyclic_named<S, O>(&mut self, name: impl Into<String>, system: S) -> SystemHandle
    where
        S: CyclicSystem<Output = Out<O>> + 'static,
        O: SystemOutput + BindPorts + 'static,
        S::Input: BindPorts + 'static,
    {
        self.graph.add_cyclic_named(name, system)
    }

    /// Register an async system under its type's `System::NAME` instance name.
    pub fn add_async<S>(&mut self, system: S) -> SystemHandle
    where
        S: AsyncSystem + 'static,
        S::Input: BindPorts + 'static,
        S::Output: BindPorts + 'static,
    {
        self.graph.add_async(system)
    }

    /// Register an async system under an explicit instance name.
    pub fn add_async_named<S>(&mut self, name: impl Into<String>, system: S) -> SystemHandle
    where
        S: AsyncSystem + 'static,
        S::Input: BindPorts + 'static,
        S::Output: BindPorts + 'static,
    {
        self.graph.add_async_named(name, system)
    }

    /// Register a pack entry under an explicit instance name, running its
    /// create phase (params decode + state construction) now so a bad config
    /// fails at registration, not at `build()`. The bind phase runs at
    /// `build()` over the entry's descriptor like any static system's.
    pub fn add_pack_entry(
        &mut self,
        name: impl Into<String>,
        entry: &mut crate::pack::PackEntry,
        params: crate::pack::EntryParams<'_>,
    ) -> Result<SystemHandle, crate::pack::MakeError> {
        self.graph.add_pack_entry(name, entry, params)
    }

    /// Register a dlopen'd cyclic system under an explicit instance name.
    /// `loaded` is an opened [`DlSystem`](crate::dl); `params` is the canonical
    /// postcard `Params` blob the `.so` decodes in `fsw_create`.
    ///
    /// The dl twin of [`add_cyclic_named`](Self::add_cyclic_named): it pushes
    /// the `.so`'s reconstructed [`SystemDescriptor`] so the ordinary
    /// `compatible()`/`WireError` validation and ring sizing run over it
    /// unchanged, and records a registration whose bind (at `build()`) gathers
    /// the per-port ring regions, `fsw_create`s the state, and produces a
    /// [`DlSlot`](crate::dl) instead of a typed `CyclicRunner`. Its output
    /// buffers land in the [`Registry`] like a static system's.
    ///
    /// Dl systems are cyclic-only. This is the low-level builder method; the
    /// [`resolve`](crate::wiring::resolve) entry point drives it from a
    /// [`Wiring`](crate::Wiring).
    pub fn add_dl_cyclic(
        &mut self,
        name: impl Into<String>,
        loaded: crate::dl::DlSystem,
        params: Vec<u8>,
    ) -> SystemHandle {
        self.graph.add_dl_cyclic(name, loaded, params)
    }

    /// Register a cross-process cyclic system under an explicit instance
    /// name: the artifact at `artifact` will be dlopen'd and driven **in a
    /// worker process** the coordinator spawns at `build()`
    /// (`docs/process-systems.md`).
    ///
    /// The process twin of [`add_dl_cyclic`](Self::add_dl_cyclic), with one
    /// deliberate difference: the host never loads the artifact, so instead
    /// of an opened [`DlSystem`](crate::dl::DlSystem) this takes the already
    /// decoded `descriptor` — obtained from a describe-mode worker run (the
    /// [`resolve`](crate::wiring::resolve) front-end does this) — and the
    /// canonical postcard `params` blob. Validation, sizing, and registry
    /// entries run over the descriptor unchanged; the rings this system
    /// touches are allocated mmap-backed in the run's session directory.
    ///
    /// Process systems are cyclic-only and need a cross-process futex;
    /// `build()` rejects the registration on unsupported targets.
    pub fn add_proc_cyclic(
        &mut self,
        name: impl Into<String>,
        descriptor: SystemDescriptor,
        artifact: PathBuf,
        system: impl Into<String>,
        params: Vec<u8>,
    ) -> SystemHandle {
        self.graph
            .add_proc_cyclic(name, descriptor, artifact, system, params)
    }

    /// Use `exe` as the worker executable for process systems instead of
    /// re-executing the host binary. For hosts whose own binary cannot serve
    /// as a worker (or wants a leaner one).
    pub fn worker_exe(&mut self, exe: impl Into<PathBuf>) -> &mut Self {
        self.graph.worker_exe = Some(exe.into());
        self
    }

    /// Root the run's shared-memory session directory at `dir` instead of
    /// the default (`/dev/shm` when present, else the OS temp dir).
    pub fn shm_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.graph.shm_dir = Some(dir.into());
        self
    }

    /// Register a runtime-swappable slot: a fixed position in the cyclic call
    /// chain whose occupant the host `Load`s/`Start`s/`Stop`s/`Abort`s/
    /// `Reset`s/`Unload`s at runtime. `allowed` is the validated candidate
    /// set, each [`AllowedOccupant`] a sequence occupant (its `Load` name,
    /// decoded descriptor, postcard params, and [`OccupantBacking`]);
    /// `initial` optionally applies one at startup.
    ///
    /// The backing decides the slot's mode, and per-slot means all-occupants:
    /// an all-[`Artifact`](OccupantBacking::Artifact) set makes this a
    /// **process slot** (`docs/process-slots.md`), whose occupants run in a
    /// worker process spawned per `Load`, with the crossing rings allocated
    /// as session-dir files. A mixed set would let `Load` silently change
    /// the slot's fault domain, so it is rejected.
    ///
    /// The registered descriptor is derived from the occupant descriptor by
    /// extension, not surgery. The occupant's ports form the *prefix* of each
    /// list in the occupant's own order (its trailing [`SlotControlIn`](crate::sequence::SlotControlIn)
    /// input re-marked [`PortConn::Host`](crate::PortConn::Host) in place,
    /// since the runner holds the cancel writer), and the runner's ports are
    /// the *tail*: a declared `commands` `MsgIn<SequenceCommand>` fan-in (an
    /// ordinary edge input, so command wiring is ordinary message wiring) plus
    /// a [`SelfTap`](crate::PortConn::SelfTap) view over the occupant's own
    /// [`SequenceStatus`](crate::sequence::SequenceStatus) output on the input
    /// side; a [`SlotStatus`] output and the `"sequences"` events channel
    /// (both `Host`, registry-tapped) on the output side. The bind arm maps the
    /// occupant `FswRing` arrays as a straight prefix walk, so the
    /// occupant-side positional bind contract (and so the dl ABI) is untouched.
    ///
    /// Errors with a [`SlotConfigError`] on a contract violation: `allowed`
    /// is empty, the occupants mix backings, an allowed occupant is not
    /// `compatible()` with the first occupant's contract, an occupant
    /// declares a mount-appended port itself, or `initial` names an occupant
    /// outside the allowed set. Wiring front-ends run the pure-spec half of
    /// these checks before opening any occupant artifact and map the rest
    /// onto their own diagnostics.
    pub fn add_slot(
        &mut self,
        name: impl Into<String>,
        allowed: Vec<AllowedOccupant>,
        initial: Option<InitialOccupant>,
    ) -> Result<SystemHandle, SlotConfigError> {
        self.graph.add_slot(name, allowed, initial)
    }

    /// Connect a producer output to a consumer input, addressed by port id.
    /// The full compatibility and structural validation runs in
    /// [`build`](Self::build); this only catches the cheap port-id and
    /// unknown-system/port mistakes early. One entry point for every edge: the
    /// edge's behavior (fan-in rule, cycle-detection membership) is inferred
    /// from the connected ports' descriptors, so a snapshot (frame) edge and a
    /// log (message) edge spell identically.
    ///
    /// This declares a forward (acyclic) edge. If a `connect` happens to close
    /// a feedback loop over snapshot edges, including a system connected to
    /// itself, `build` rejects it as a
    /// [`FeedbackCycle`](WireError::FeedbackCycle); the back-edge of a loop
    /// must be declared with [`connect_delayed`](Self::connect_delayed).
    /// Likewise a snapshot edge between two cyclic systems must point forward
    /// in registration order (the step loop's execution order), or the
    /// consumer would permanently read last cycle's value; `build` rejects the
    /// backward edge as a [`StaleFrameEdge`](WireError::StaleFrameEdge) unless
    /// it is `connect_delayed`. Log edges are exempt from both (a decoupled
    /// event/command stream).
    pub fn connect(&mut self, producer: PortRef, consumer: PortRef) -> Result<(), WireError> {
        init::check_edge(&self.graph.systems, producer, consumer)?;
        self.graph.push_edge(producer, consumer, false);
        Ok(())
    }

    /// Connect a producer to a consumer, marking the edge as an intentional
    /// one-cycle-delayed feedback edge (the back-edge of a control loop). The
    /// runtime path is identical to [`connect`](Self::connect), a `view()`
    /// read of the latest committed value, which is last cycle's because the
    /// producer runs after the consumer in registration order; but the edge is
    /// excluded from cycle detection, so the loop builds. Every feedback loop
    /// must break exactly one edge this way; an unbroken cycle is a
    /// [`FeedbackCycle`](WireError::FeedbackCycle). Only meaningful on a
    /// snapshot edge; `delayed` into a log input is rejected at build as a
    /// [`DelayedLogEdge`](WireError::DelayedLogEdge).
    pub fn connect_delayed(
        &mut self,
        producer: PortRef,
        consumer: PortRef,
    ) -> Result<(), WireError> {
        init::check_edge(&self.graph.systems, producer, consumer)?;
        self.graph.push_edge(producer, consumer, true);
        Ok(())
    }

    /// Validate the graph, size and allocate every ring, bind ports,
    /// auto-provision health/log buffers, and return a ready coordinator. The
    /// pass chain lives in [`init::build`].
    pub fn build(self) -> Result<Coordinator, WireError> {
        init::build(self.graph)
    }
}

// ---------------------------------------------------------------------------
// bind() products
// ---------------------------------------------------------------------------

/// The coordinator's own (#0) bound ports, wrapped by [`bind_coordinator`].
struct CoordinatorPorts {
    health: HealthPort,
    status_out: Output<CoordinatorStatus>,
    seq_registry_out: MsgOut<SequenceRegistry>,
    control_out: MsgOut<SequenceCommand>,
    reload_in: MsgIn<ReloadSequences>,
    /// The `wiring` writer, present only when a front-end set a manifest (so
    /// the #0 bundle declared the port).
    wiring_out: Option<MsgOut<WiringManifest>>,
}

/// The bind pass product: every cyclic slot, every pending async system, and
/// the coordinator's own ports.
struct BoundSystems {
    cyclic: Vec<Box<dyn CyclicSlot>>,
    pending_async: Vec<PendingAsync>,
    coord: CoordinatorPorts,
}

/// The `Simulated` per-cycle timestamp, `epoch + k*dt`, computed in wide
/// integer nanoseconds so the cycle index is never truncated (a narrower
/// `dt * k as u32` would wrap the timeline back to `epoch` every 2³² cycles,
/// breaking monotonicity and stalling in-flight `Wait`s). The u128 product
/// cannot overflow for any realistic run; the final i64-microsecond cast holds
/// roughly 292k years of simulated time.
fn simulated_now(epoch: Timestamp, dt: Duration, k: u64) -> Timestamp {
    Timestamp(epoch.0 + (dt.as_nanos() * k as u128 / 1_000) as i64)
}

/// Whether the freshly scanned stopped set differs from the previously
/// published one. Both slices come from the same in-order scan of the cyclic
/// slots, so an element-wise `(name, reason)` compare is an exact membership
/// compare, with no set structure and no allocation. A length-only check is
/// not enough: stops are not monotonic (a slot recovers via `Load`/`Reset`),
/// so slot A can recover the same cycle slot B stops, changing the membership
/// while the count stays put.
fn stopped_set_changed(cur: &[StoppedSystem], prev: &[StoppedSystem]) -> bool {
    cur.len() != prev.len()
        || cur
            .iter()
            .zip(prev)
            .any(|(a, b)| a.name != b.name || a.reason != b.reason)
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

/// The wired, ready flight-software graph. Drives cyclic systems once per
/// cycle, runs the async copy-in step, spawns and tears down async systems,
/// and emits coordinator-level health plus a status frame.
pub struct Coordinator {
    config: CoordinatorConfig,
    cyclic: Vec<Box<dyn CyclicSlot>>,
    pending_async: Vec<PendingAsync>,
    copy_ins: Vec<CopyIn>,
    coord_health: HealthPort,
    status_out: Output<CoordinatorStatus>,
    stopped: Vec<StoppedSystem>,
    /// Scratch for `update_status`'s per-cycle scan, swapped with `stopped` on
    /// a change, so the hot loop never allocates.
    stopped_scratch: Vec<StoppedSystem>,
    /// The process systems' worker facts as of the last change, mirrored into
    /// the status frame's worker list. Empty without process systems.
    workers: Vec<WorkerStatus>,
    /// Scratch twin of `workers`, as for `stopped_scratch`.
    workers_scratch: Vec<WorkerStatus>,
    cycle: u64,
    /// A shared, lock-free mirror of `cycle` that an observer outside the loop
    /// can read while `run_for` holds `&mut self`, published each cycle.
    /// Purely observational; nothing in the loop reads it back.
    progress: Arc<AtomicU64>,
    /// The one broad registry over every registered buffer, frames and message
    /// channels alike, untelemetered entries included.
    registry: Arc<Registry>,
    /// The single writer over the coordinator's declared `commands` output
    /// (its #0 bundle): the in-proc `SequenceCommand` producer. A slot reads
    /// it only over an explicit `"coordinator" -> <slot>` edge, the same
    /// wiring surface an uplink uses; with no edge the handle is inert but
    /// visible in the graph. Minted once at `build()` (the ring's writer claim
    /// enforces one live writer) and handed out by
    /// [`control_handle`](Coordinator::control_handle), which takes it, `None`
    /// afterwards.
    control_out: Option<MsgOut<SequenceCommand>>,
    /// The sole writer of the coordinator's boot-`SequenceRegistry` message channel.
    seq_registry_out: MsgOut<SequenceRegistry>,
    /// The prebuilt boot [`SequenceRegistry`] payload (the slots plus their
    /// allowed occupants), emitted once at the head of
    /// [`run_for`](Coordinator::run_for).
    seq_registry: SequenceRegistry,
    /// Whether the boot `SequenceRegistry` has been emitted (emit-once; the
    /// re-emit hook is [`emit_sequence_registry`](Coordinator::emit_sequence_registry)).
    seq_registry_emitted: bool,
    /// The sole writer of the coordinator's `wiring` channel, present only when
    /// a front-end supplied a manifest.
    wiring_out: Option<MsgOut<WiringManifest>>,
    /// The full mission IR to broadcast on the `wiring` channel, emitted once
    /// at the head of [`run_for`](Coordinator::run_for) and re-fired on a
    /// [`ReloadSequences`] request. `None` mirrors `wiring_out`.
    wiring_manifest: Option<WiringManifest>,
    /// Whether the [`WiringManifest`] has been emitted (emit-once; re-emitted
    /// on reload alongside the `SequenceRegistry`).
    wiring_emitted: bool,
    /// The [`ReloadSequences`] fan-in on the coordinator's #0 bundle, drained
    /// each cycle: any request re-emits the `SequenceRegistry`, so a consumer
    /// that connected after boot (a late-started panel) can recover the
    /// channel list on demand.
    reload_in: MsgIn<ReloadSequences>,
    /// Latched by the first [`run_for`](Coordinator::run_for): a run consumes
    /// the coordinator (spawned async systems and their transports are gone
    /// after shutdown), so a second run would silently re-init everything over
    /// dead plumbing. It panics instead.
    started: bool,
    /// Canonical ring handles; declared last so they drop after every port.
    #[allow(dead_code)]
    rings: RingTable,
    /// The run's shared-memory session directory (`None` without process
    /// systems). After `rings`, so the files are unmapped before the
    /// directory is removed; the slots (and their workers) died earlier
    /// still, in `cyclic`'s drop.
    #[allow(dead_code)]
    session: Option<SessionDir>,
}

impl Coordinator {
    /// Start a builder.
    pub fn builder(config: CoordinatorConfig) -> CoordinatorBuilder {
        CoordinatorBuilder::new(config)
    }

    /// The process systems' worker facts (pid, restarts, run state) as of the
    /// last change, as also published in the coordinator status frame's
    /// worker list. Empty for a graph without process systems.
    pub fn workers(&self) -> &[WorkerStatus] {
        &self.workers
    }

    /// The cyclic systems that have hard-stopped, as also published in the
    /// coordinator status frame.
    pub fn stopped(&self) -> &[StoppedSystem] {
        &self.stopped
    }

    /// A shared handle to the live cycle counter (0 before the first cycle),
    /// readable while [`run_for`](Self::run_for) is running, for a progress
    /// heartbeat on another task. Lock-free, observational only.
    pub fn progress(&self) -> Arc<AtomicU64> {
        self.progress.clone()
    }

    /// The one broad registry over every registered buffer: an index a logger,
    /// recorder, debugger, or test can use to read any buffer, frame output or
    /// message channel, by its instance-qualified id
    /// `ComponentId::new("<instance>.<name>")`. Unfiltered: untelemetered
    /// entries (`coordinator.commands`, for example) are visible here, unlike
    /// through [`AllOutputs`](crate::AllOutputs).
    pub fn registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }

    /// Emit the boot [`SequenceRegistry`] on the coordinator's message
    /// channel: the slots and their allowed occupants. Called once at the head
    /// of [`run_for`](Self::run_for); exposed as the re-emit hook for a
    /// rebuilt payload.
    pub fn emit_sequence_registry(&mut self) {
        let _ = self.seq_registry_out.emit(&self.seq_registry);
    }

    /// Emit the full mission IR on the coordinator's `wiring` channel — the
    /// live/historical topology the panel graph tile consumes. A no-op when no
    /// front-end set a manifest. Called once at the head of
    /// [`run_for`](Self::run_for) and re-fired on a [`ReloadSequences`]
    /// request, so a consumer that connected after boot resyncs on demand.
    pub fn emit_wiring_manifest(&mut self) {
        if let (Some(out), Some(manifest)) = (&mut self.wiring_out, &self.wiring_manifest) {
            let _ = out.emit(manifest);
        }
    }

    /// The writer over the coordinator's command channel: the in-proc
    /// convenience for driving slots `Load`/`Start`/`Stop`/`Abort`/`Reset`.
    /// The host or a test [`emit`](MsgOut::emit)s [`SequenceCommand`]s the
    /// slots drain once per cycle, the same mechanism an uplink system uses,
    /// just over an in-proc channel instead of a wire one. Address a slot by
    /// its instance name (`SequenceCommand::channel`), the same key the
    /// wiring, the telemetry prefix, and the boot [`SequenceRegistry`] use.
    ///
    /// The channel has exactly one writer (the ring's writer claim enforces
    /// it), minted at `build()` and handed out here once: the first call
    /// returns it, every later call returns `None`. Take it once and hold it
    /// for the run, so driving commands from one place is structural.
    ///
    /// Commands reach a slot only over an explicit `"coordinator" -> <slot>`
    /// edge; with no edge the handle is inert but the wiring shows it, so the
    /// gap is diagnosable from the graph.
    pub fn control_handle(&mut self) -> Option<MsgOut<SequenceCommand>> {
        self.control_out.take()
    }

    /// Every owned output buffer as `(instance-name, frame-id)`. The instance
    /// name is the unique per-system handle a telemetry sink prefixes records
    /// with (`<instance>.<frame>.<component>`), so two instances of one system
    /// type emit distinct fully-qualified paths despite sharing a `frame_id`.
    pub fn output_instances(&self) -> Vec<(&str, ComponentId)> {
        self.rings
            .rings
            .iter()
            .filter(|e| matches!(e.role, BufferRole::Output { .. }))
            .filter_map(|e| e.instance.as_deref().map(|name| (name, e.frame_id)))
            .collect()
    }

    /// Run the lifecycle for a bounded number of cycles: init all (behind the
    /// barrier), run, then shut all down. Convenient for tests and bounded
    /// missions.
    ///
    /// # Panics
    ///
    /// Panics if called a second time on the same coordinator: a run
    /// *consumes* it. The async systems were moved into their (now torn-down)
    /// tasks and a transport's connection is gone, so a rerun would re-init
    /// every cyclic system over dead plumbing. Build a fresh `Coordinator` to
    /// run again.
    pub async fn run_for(&mut self, cycles: usize) {
        assert!(
            !self.started,
            "Coordinator::run_for called twice — a coordinator drives exactly one run \
             (its async systems/transports are consumed by the first); build a fresh \
             Coordinator to run again"
        );
        self.started = true;
        let tasks = self.start().await;
        // Emit the boot `SequenceRegistry` once, before the first cycle's
        // events flow, so a tap claimed after `build()` observes it ahead of
        // any `SequenceChannelEvent`.
        if !self.seq_registry_emitted {
            self.emit_sequence_registry();
            self.seq_registry_emitted = true;
        }
        // The wiring manifest rides the same boot path: emit once before the
        // first cycle so a tap claimed after `build()` sees the topology ahead
        // of any live edge activity.
        if !self.wiring_emitted {
            self.emit_wiring_manifest();
            self.wiring_emitted = true;
        }
        // The Wall pacing budget. Only computed under a `Wall` clock, since
        // `cycle_rate` is documented ignored under `Simulated` and an unusable
        // rate must not panic there; under `Wall` the rate was validated at
        // `build()` (`InvalidCycleRate`), so the conversion cannot panic.
        let budget = match self.config.clock {
            ClockMode::Wall => Duration::from_secs_f64(1.0 / self.config.cycle_rate),
            ClockMode::Simulated { .. } => Duration::ZERO,
        };
        // The epoch a `Simulated` clock advances from; unused under `Wall`.
        let epoch = Timestamp::now();
        for k in 0..cycles {
            let start = Instant::now();
            self.cycle += 1;
            // Publish progress for any observer outside the loop.
            self.progress.store(self.cycle, Relaxed);
            // The per-cycle timestamp every system shares: wall time, or the
            // simulated clock at `epoch + k*dt`.
            let now = match self.config.clock {
                ClockMode::Wall => Timestamp::now(),
                ClockMode::Simulated { dt } => simulated_now(epoch, dt, k as u64),
            };
            // A reload request re-emits the SequenceRegistry for consumers
            // that missed the boot message; the drain coalesces a burst of
            // requests into one emission per cycle.
            let mut reload = false;
            self.reload_in.drain(|ReloadSequences {}| reload = true);
            if reload {
                self.emit_sequence_registry();
                // Topology does not change on reload today, but a slot-occupancy
                // consumer that missed boot resyncs off the same one message.
                self.emit_wiring_manifest();
            }
            // Commands are drained per-slot at the head of each `step`: a
            // slot's declared `commands` fan-in reads exactly the producers
            // explicitly edged into it and filters by its instance name, so a
            // command dispatches the *same* cycle it lands, with no
            // coordinator-side command stage.
            for slot in &mut self.cyclic {
                slot.step(now);
            }
            self.run_copy_ins();
            self.update_status(now);
            match self.config.clock {
                // Wall: sleep out the remainder of the cycle budget.
                ClockMode::Wall => {
                    let elapsed = start.elapsed();
                    if elapsed < budget {
                        stellarator::sleep(budget - elapsed).await;
                    } else {
                        self.telemeter_overrun(now, elapsed, budget);
                    }
                }
                // Simulated: no pacing, run as fast as possible. Still yield
                // once so any spawned async consumer (driven by the copy-in
                // above) gets to run on this cooperative runtime.
                ClockMode::Simulated { .. } => stellarator::yield_now().await,
            }
        }
        self.shutdown(tasks).await;
    }

    /// Phase 1: spawn async systems (each inits and signals readiness), wait
    /// for the init barrier, run cyclic inits, then release the async tasks.
    /// Holds the barrier so every `init` completes before the first cycle or
    /// any `run` pass.
    async fn start(&mut self) -> Vec<AsyncTask> {
        let n_async = self.pending_async.len();
        let ready = Arc::new(WaitQueue::new());
        let ready_count = Arc::new(AtomicUsize::new(0));
        let go = Arc::new(WaitQueue::new());
        let go_flag = Arc::new(AtomicBool::new(false));

        let mut tasks = Vec::with_capacity(n_async);
        for pending in std::mem::take(&mut self.pending_async) {
            let stop = Arc::new(AtomicBool::new(false));
            let ctx = LaunchCtx {
                stop: stop.clone(),
                ready: ready.clone(),
                ready_count: ready_count.clone(),
                go: go.clone(),
                go_flag: go_flag.clone(),
            };
            let handle = pending.launcher.launch(ctx);
            tasks.push(AsyncTask {
                handle: handle.drop_guard(),
                stop,
                wake_on_stop: pending.wake_on_stop,
            });
        }

        // Barrier: wait for every async system's init to complete.
        if n_async > 0 {
            let _ = ready
                .wait_for(|| ready_count.load(Acquire) == n_async)
                .await;
        }
        // Cyclic inits run on the loop's task before the first cycle.
        for slot in &mut self.cyclic {
            slot.init();
        }

        // Release the async tasks into their run loops.
        go_flag.store(true, Release);
        go.wake_all();
        tasks
    }

    /// Mirror the newest upstream record into each async system's private
    /// buffer, waking the async `recv`. Snapshot semantics: older unread
    /// upstream records are consumed on the way (freed for the producer) and
    /// only the newest is mirrored, at most once per new upstream commit. A
    /// full private ring (the consumer is behind) skips this cycle's mirror;
    /// the next cycle retries with whatever is newest then.
    fn run_copy_ins(&mut self) {
        for c in &mut self.copy_ins {
            // Skip untouched upstreams: `committed` moves iff a record landed
            // on this ring, so this also keeps the pinned newest record from
            // being re-mirrored (and the consumer re-woken) every cycle.
            let committed = c.upstream.committed();
            if committed == c.last_committed {
                continue;
            }
            c.last_committed = committed;
            // Corrupt (unreachable from in-crate behavior) reads as "nothing new".
            if let Ok(Some(grant)) = c.upstream.try_latest() {
                let _ = c.writer.try_write(&grant);
            }
        }
    }

    /// Scan the slots; when the stopped set changes, refresh the status frame
    /// and log the change to coordinator health. The scan fills a retained
    /// scratch and swaps it with `stopped` on a change, so nothing allocates
    /// per cycle.
    fn update_status(&mut self, now: Timestamp) {
        // Host-side worker trouble (a step missing its ack deadline, a
        // restart beginning) lands on coordinator health: the worker owns its
        // system's health ring, so the host cannot report through it. At most
        // one of each per slot per cycle.
        let mut worker_event = false;
        for slot in &mut self.cyclic {
            if slot.drain_timeouts() > 0 {
                self.coord_health.error("proc_step_timeout");
                self.coord_health.log(Level::Warn, slot.name());
                worker_event = true;
            }
            if slot.drain_restarts() > 0 {
                self.coord_health.error("proc_restart");
                self.coord_health.log(Level::Warn, slot.name());
                worker_event = true;
            }
        }
        if worker_event {
            self.coord_health.end_cycle(now, 0);
        }
        self.stopped_scratch.clear();
        self.workers_scratch.clear();
        for slot in &self.cyclic {
            // Only a hard stop is an error-stop; a runtime slot's
            // Empty/Loaded/Done states are not (the `stop_reason` projection).
            if let Some(reason) = slot.state().stop_reason() {
                self.stopped_scratch.push(StoppedSystem {
                    name: Arc::from(slot.name()),
                    reason,
                });
            }
            if let Some(status) = slot.worker_status() {
                self.workers_scratch.push(status);
            }
        }
        let stopped_changed = stopped_set_changed(&self.stopped_scratch, &self.stopped);
        // Worker changes (a pid after a restart, a run-state transition) also
        // re-publish, so the wire always names the current process.
        let workers_changed = self.workers_scratch != self.workers;
        if !stopped_changed && !workers_changed {
            return;
        }
        core::mem::swap(&mut self.stopped, &mut self.stopped_scratch);
        core::mem::swap(&mut self.workers, &mut self.workers_scratch);
        self.publish_status(now);
        if stopped_changed {
            for i in 0..self.stopped.len() {
                let name = self.stopped[i].name.clone();
                self.coord_health.error("system_stopped");
                self.coord_health.log(Level::Warn, &name);
            }
            self.coord_health.end_cycle(now, 0);
        }
    }

    fn publish_status(&mut self, now: Timestamp) {
        let frame = CoordinatorStatus {
            timestamp: now,
            cycle: self.cycle,
            stopped_count: self.stopped.len() as u64,
            worker_count: self.workers.len() as u64,
            stopped: FrameList::EMPTY,
            workers: FrameList::EMPTY,
        };
        // Split borrows: the writer takes `status_out`, the closure reads
        // `stopped`/`workers`, so no intermediate entries Vec is needed.
        let (stopped, workers) = (&self.stopped, &self.workers);
        let _ = self.status_out.write_with(&frame, |fw| {
            fw.list(offset_of!(CoordinatorStatus, stopped), |l| {
                for sys in stopped {
                    let (name, len) = pack_name(&sys.name);
                    l.push(StoppedEntry {
                        reason: sys.reason.code(),
                        len,
                        _pad: [0; 6],
                        name,
                    });
                }
            });
            fw.list(offset_of!(CoordinatorStatus, workers), |l| {
                for w in workers {
                    let (name, len) = pack_name(&w.name);
                    l.push(WorkerEntry {
                        pid: w.pid,
                        restarts: w.restarts,
                        state: w.state.code(),
                        len,
                        _pad: [0; 6],
                        name,
                    });
                }
            });
        });
    }

    fn telemeter_overrun(&mut self, now: Timestamp, elapsed: Duration, budget: Duration) {
        self.coord_health.error("cycle_overrun");
        self.coord_health.log(
            Level::Warn,
            &format!(
                "cycle overran: {}us > {}us",
                elapsed.as_micros(),
                budget.as_micros()
            ),
        );
        self.coord_health.end_cycle(now, elapsed.as_micros() as u64);
    }

    /// Cooperative teardown: signal every async task, wake any parked `recv`,
    /// give the tasks a brief window to finish their current pass and run
    /// their own `shutdown`, then drop the tasks, whose `drop_guard` cancels
    /// any still parked. Finally `shutdown` the cyclic systems in reverse
    /// registration order. The `RingTable` drops last (struct field order).
    async fn shutdown(&mut self, tasks: Vec<AsyncTask>) {
        for t in &tasks {
            t.stop.store(true, Release);
            for n in &t.wake_on_stop {
                n.notify();
            }
        }
        // A task parked in `Input::recv` cannot be woken without data (the
        // wait re-checks for a committed record), so a recv-driven loop only
        // exits on the next datum; the bounded window lets timer- and
        // data-paced tasks observe `stop` and flush in `System::shutdown`
        // before we cancel.
        stellarator::sleep(JOIN_TIMEOUT).await;
        drop(tasks);
        for slot in self.cyclic.iter_mut().rev() {
            slot.shutdown();
        }
    }
}

#[cfg(test)]
mod tests;

// Scripted-uplink command dispatch, driven through the builder with a
// test-double transport the Wiring front end cannot express (WP3). Gated off
// miri, since two of the tests cross a real shared-object boundary.
#[cfg(all(test, not(miri)))]
mod uplink_tests;
