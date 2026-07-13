//! Build-time wiring and the cyclic run loop.
//!
//! A [`CoordinatorBuilder`] collects systems and the edges between their ports.
//! Its `build()` validates the graph, sizes and allocates one ring per output
//! port, and binds every port over those rings, producing a ready
//! [`Coordinator`]. [`Coordinator::run_for`] then drives the lifecycle: spawn
//! the async systems, init everything behind a barrier, step the cyclic
//! systems once per cycle, and tear it all down.
//!
//! # Execution order
//!
//! Cyclic systems step in registration order, once per cycle. A snapshot edge
//! only observes the current cycle's value when it points forward in that
//! order, so `build()` rejects a backward snapshot edge between cyclic systems
//! ([`WireError::StaleFrameEdge`]) and any feedback loop not broken by an
//! explicit [`connect_delayed`](CoordinatorBuilder::connect_delayed) edge
//! ([`WireError::FeedbackCycle`]). One-cycle-late sampling is therefore always
//! a declared decision, never an accident of registration order. Log edges
//! carry decoupled event/command streams with no same-cycle dependency and are
//! exempt from both rules, as are edges touching an async endpoint.
//!
//! # Port connections
//!
//! An input's [`PortConn`] says who feeds it. An `Edge` input is wired by
//! [`connect`](CoordinatorBuilder::connect). A `Host` input's counterpart is
//! held by the system's runner over a dedicated ring (a slot's cancel frame,
//! for example). A `SelfTap` input is a read view over one of the system's own
//! outputs. Edges into `Host` or `SelfTap` inputs are rejected; `Host`
//! *outputs* still accept consumer edges (the coordinator's own command
//! channel is one).
//!
//! # Async systems and copy-ins
//!
//! An async system runs on its own task, off the cycle clock, so it cannot be
//! step-gated. Each of its edge-connected snapshot inputs is decoupled through
//! a private ring: after the cyclic step loop, the coordinator mirrors the
//! newest upstream record into the private ring, whose data notifier wakes the
//! task's parked `recv`. Log inputs need no copy-in; they read the producers'
//! rings directly and are poll-drained.
//!
//! # Reader budgets
//!
//! Every ring's `max_readers` is fixed at build time. The budget is the
//! counted edge fan-out plus declared self-taps, one slot per receive-all
//! capability in the graph, and [`CoordinatorConfig::reader_slack`] spare
//! slots for taps claimed through the [`Registry`] after build. Exhausting the
//! budget surfaces as an error at the claim site, not a panic.
//!
//! # The coordinator's own bundle
//!
//! The coordinator registers itself as system #0 under the reserved instance
//! name `"coordinator"`, declaring its own channels (health, log, status, the
//! boot sequence registry, the operator command channel) as an ordinary
//! descriptor. They are validated, sized, allocated, and registered by the
//! same passes as every other system's; the bind pass wraps the allocated
//! rings into the coordinator's fields instead of a cyclic slot, because the
//! coordinator is the loop rather than a member of it.

use core::mem::offset_of;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::time::{Duration, Instant};

use metor_fsw_ring::{Config, NoWake, Notifier, RingBuffer, View, WakeSource, Writer};
use metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_proto_wkt::{
    ReloadSequences, SequenceChannelSpec, SequenceCommand, SequenceRegistry, WiringManifest,
};
use stellarator::sync::WaitQueue;
use stellarator::{JoinHandle, JoinHandleDropGuard};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::binder::{BindPorts, Binder};
use crate::descriptor::compatible;
use crate::descriptor::{
    Delivery, FanIn, Hz, PortConn, PortDesc, PortId, PortSchema, SystemDescriptor, SystemKind,
};
use crate::dynamic::FrameList;
use crate::health::{HealthPort, Level};
use crate::message::{LOG_DEPTH, MAX_MSG_BYTES, MsgIn, MsgOut};
use crate::port::{Output, capacity_for};
use crate::proc::session::SessionDir;
use crate::registry::{EntrySchema, Registry, RegistryEntry};
use crate::system::{AsyncSystem, CyclicRunner, CyclicSystem, Out, System, SystemOutput};
use crate::{DEFAULT_DEPTH, Frame};

mod bind;
mod error;
mod slot;
mod status;
use bind::{ProcBindCtx, bind_systems};
pub use error::WireError;
pub(crate) use slot::validate_slot_spec;
pub use slot::{
    AllowedOccupant, InitialOccupant, OccupantBacking, SlotConfigError, SlotStatus,
};
use slot::SlotReg;
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
// Registration (type erasure of the boxed systems)
// ---------------------------------------------------------------------------

trait CyclicRegistration {
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot>;
}

struct CyclicReg<S> {
    system: S,
}

impl<S, O> CyclicRegistration for CyclicReg<S>
where
    S: CyclicSystem<Output = Out<O>> + 'static,
    O: SystemOutput + BindPorts + 'static,
    S::Input: BindPorts + 'static,
{
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot> {
        // The host binds over its own pre-allocated heap rings via the `Binder`
        // ring source; a dlopen'd system runs the identical (backing-erased)
        // bind on its own side of the ABI over non-owning attaches.
        let input = <S::Input as BindPorts>::bind(binder);
        let output = <Out<O> as BindPorts>::bind(binder);
        Box::new(CyclicRunner::new(self.system, input, output))
    }
}

trait AsyncRegistration {
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn AsyncLauncher>;
}

struct AsyncReg<S> {
    system: S,
}

impl<S> AsyncRegistration for AsyncReg<S>
where
    S: AsyncSystem + 'static,
    S::Input: BindPorts + 'static,
    S::Output: BindPorts + 'static,
{
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn AsyncLauncher> {
        let input = <S::Input as BindPorts>::bind(binder);
        let output = <S::Output as BindPorts>::bind(binder);
        Box::new(AsyncSlot {
            system: self.system,
            input,
            output,
        })
    }
}

/// A registered dlopen'd cyclic system: the loaded handle plus its postcard
/// `Params` blob. At `build()` it becomes a [`DlSlot`](crate::dl) instead of a
/// typed [`CyclicRunner`]; everything before that (descriptor push, edge
/// validation, ring sizing/allocation, registry entry) is the same as the
/// static-system path.
struct DlReg {
    system: crate::dl::DlSystem,
    params: Vec<u8>,
}

/// A registered process system: the artifact path a worker process will
/// dlopen plus its postcard `Params` blob. The descriptor (on the enclosing
/// [`Registration`]) arrived as decoded describe-worker bytes — the host
/// holds no `DlSystem` and never loads the artifact itself. At `build()` it
/// becomes a [`ProcSlot`](crate::proc); everything before that is the same
/// uniform pass as every other kind.
struct ProcReg {
    artifact: PathBuf,
    params: Vec<u8>,
    /// The pack entry the worker instantiates.
    system: String,
}

/// A registered pack entry, created (params decoded, state built) at
/// registration; the pending half binds its ports at `build()` and yields
/// the boxed [`Driver`](crate::pack::Driver) a [`DriverSlot`] steps.
struct PackReg {
    pending: crate::pack::Pending,
    /// The entry's own name, the slot's static display name (the instance
    /// name lives on the enclosing [`Registration`], as for every kind).
    entry_name: &'static str,
}

enum Reg {
    Cyclic(Box<dyn CyclicRegistration>),
    Async(Box<dyn AsyncRegistration>),
    /// A pack entry, bound to a [`DriverSlot`](crate::pack::DriverSlot) at
    /// `build()`.
    Pack(PackReg),
    /// A dlopen'd cyclic system, bound to a [`DlSlot`](crate::dl) at `build()`.
    Dl(DlReg),
    /// A cross-process cyclic system, spawned as a worker and bound to a
    /// [`ProcSlot`](crate::proc) at `build()`.
    Proc(ProcReg),
    /// A runtime-swappable slot, bound to a [`SlotRunner`](slot::SlotRunner) at `build()`.
    Slot(SlotReg),
    /// The coordinator itself, registered as system #0 under the reserved
    /// instance name `"coordinator"`. A marker registration: its declared
    /// outputs are allocated and registered by the uniform passes like any
    /// system's, but it is never pushed into `cyclic` (the coordinator is the
    /// loop); the bind arm wraps the allocated rings into the coordinator's own
    /// fields instead.
    Coordinator,
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// One registered system: its type-erased registration, its registered
/// descriptor (what `build()` validates, sizes, and wires), and its instance
/// name (defaults to `System::NAME`; a wiring file supplies a distinct name
/// per instance).
struct Registration {
    reg: Reg,
    desc: SystemDescriptor,
    name: String,
}

/// Registers systems and edges, then `build`s a ready [`Coordinator`].
pub struct CoordinatorBuilder {
    config: CoordinatorConfig,
    systems: Vec<Registration>,
    /// Each registered edge `(producer, consumer, delayed)`. A `delayed` edge
    /// is an intentional one-cycle-delayed feedback edge, excluded from cycle
    /// detection.
    edges: Vec<(PortRef, PortRef, bool)>,
    /// Override of the worker executable process systems spawn; `None`
    /// re-executes the host binary (see [`worker_exe`](Self::worker_exe)).
    worker_exe: Option<PathBuf>,
    /// Override of the shared-memory session root; `None` picks `/dev/shm`
    /// when present, else the OS temp dir (see [`shm_dir`](Self::shm_dir)).
    shm_dir: Option<PathBuf>,
    /// The mission IR to broadcast as a [`WiringManifest`], set by
    /// [`set_wiring_manifest`](Self::set_wiring_manifest). `Some` adds a
    /// `wiring` output to the coordinator #0 bundle at `build()`, sized from
    /// the concrete payload; `None` (a builder used without a front-end)
    /// leaves the coordinator with no wiring channel.
    wiring_manifest: Option<WiringManifest>,
}

impl CoordinatorBuilder {
    fn new(config: CoordinatorConfig) -> Self {
        let mut b = Self {
            config,
            systems: Vec::new(),
            edges: Vec::new(),
            worker_exe: None,
            shm_dir: None,
            wiring_manifest: None,
        };
        // The coordinator registers itself as system #0 under the reserved
        // instance name `"coordinator"`: an ordinary declared bundle, so its
        // channels are wired, sized, and registered by the same passes as
        // every system's. Every output is host-connected (the coordinator
        // itself holds the writers; a Host OUTPUT still accepts consumer
        // edges); the registry keys are `coordinator.health` / `.log` /
        // `.coordinator_status` / `.sequences` / `.commands`.
        //
        // - `commands` is the operator channel behind the take-once
        //   [`Coordinator::control_handle`]; commands reach a slot only over an
        //   explicit `"coordinator" -> <slot>` edge. Untelemetered, since
        //   inbound control is never echoed on the downlink.
        // - `sequences` carries the boot `SequenceRegistry`, telemetered so
        //   downstream consumers can list the channels; the `ReloadSequences`
        //   fan-in is its request channel (an ordinary edge input, zero edges
        //   legal), drained each cycle to re-emit the registry on demand for
        //   consumers that missed the boot message.
        let desc = SystemDescriptor {
            name: COORDINATOR_INSTANCE,
            kind: SystemKind::Cyclic,
            inputs: vec![PortDesc::msg::<ReloadSequences>()],
            outputs: vec![
                PortDesc::of::<crate::SystemHealth>().with_conn(PortConn::Host),
                PortDesc::of::<crate::SystemLog>().with_conn(PortConn::Host),
                PortDesc::of::<CoordinatorStatus>().with_conn(PortConn::Host),
                PortDesc::msg_named::<SequenceRegistry>("sequences").with_conn(PortConn::Host),
                PortDesc::msg_named::<SequenceCommand>("commands")
                    .untelemetered()
                    .with_conn(PortConn::Host),
            ],
            capabilities: Vec::new(),
        };
        b.push_system(desc, COORDINATOR_INSTANCE.to_string(), Reg::Coordinator);
        b
    }

    /// Record one registration; the returned handle indexes `systems`.
    fn push_system(&mut self, desc: SystemDescriptor, name: String, reg: Reg) -> SystemHandle {
        let id = self.systems.len();
        self.systems.push(Registration { reg, desc, name });
        SystemHandle { id }
    }

    /// The handle addressing the coordinator's own system-#0 bundle, so a
    /// front-end can wire the operator command edge with
    /// `connect(PortRef::msg::<SequenceCommand>(b.coordinator_handle()), …)`.
    pub fn coordinator_handle(&self) -> SystemHandle {
        SystemHandle { id: 0 }
    }

    /// Broadcast `manifest` as a [`WiringManifest`] at startup and on reload.
    ///
    /// The front-end ([`resolve`](crate::wiring::resolve)) hands over the full,
    /// path-stripped mission IR here; `build()` adds a `wiring` output to the
    /// coordinator #0 bundle, sized from the concrete JSON payload (which for a
    /// non-trivial mission exceeds [`MAX_MSG_BYTES`]), and the run loop emits it
    /// on the telemetry plane — the pattern [`SequenceRegistry`] uses. Called
    /// again, the latest manifest wins.
    pub fn set_wiring_manifest(&mut self, manifest: WiringManifest) {
        self.wiring_manifest = Some(manifest);
    }

    /// The registered descriptor of `handle`, which is what `build()`
    /// validates, sizes, and wires. For a slot this is the derived contract
    /// (see [`add_slot`](Self::add_slot)), which a front-end reads back
    /// instead of re-deriving; for everything else it is the system's own
    /// `descriptor()`.
    pub fn descriptor_of(&self, handle: SystemHandle) -> &SystemDescriptor {
        &self.systems[handle.id].desc
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
        self.add_cyclic_named(<S as System>::NAME, system)
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
        // The instance descriptor, not the static one: a system whose port set
        // depends on its config registers what this instance actually carries.
        let desc = system.instance_descriptor();
        self.push_system(
            desc,
            name.into(),
            Reg::Cyclic(Box::new(CyclicReg { system })),
        )
    }

    /// Register an async system under its type's `System::NAME` instance name.
    pub fn add_async<S>(&mut self, system: S) -> SystemHandle
    where
        S: AsyncSystem + 'static,
        S::Input: BindPorts + 'static,
        S::Output: BindPorts + 'static,
    {
        self.add_async_named(<S as System>::NAME, system)
    }

    /// Register an async system under an explicit instance name.
    pub fn add_async_named<S>(&mut self, name: impl Into<String>, system: S) -> SystemHandle
    where
        S: AsyncSystem + 'static,
        S::Input: BindPorts + 'static,
        S::Output: BindPorts + 'static,
    {
        // The instance descriptor, not the static one (see `add_cyclic_named`).
        let desc = system.instance_descriptor();
        self.push_system(desc, name.into(), Reg::Async(Box::new(AsyncReg { system })))
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
        let pending = entry.create(params)?;
        Ok(self.push_system(
            entry.descriptor().clone(),
            name.into(),
            Reg::Pack(PackReg {
                pending,
                entry_name: entry.name(),
            }),
        ))
    }

    /// Register a dlopen'd cyclic system under an explicit instance name.
    /// `loaded` is an opened [`DlSystem`](crate::dl); `params` is the canonical
    /// postcard `Params` blob the `.so` decodes in `fsw_create`.
    ///
    /// The dl twin of [`add_cyclic_named`](Self::add_cyclic_named): it pushes
    /// the `.so`'s reconstructed [`SystemDescriptor`] so the ordinary
    /// `compatible()`/`WireError` validation and ring sizing run over it
    /// unchanged, and records a [`Reg::Dl`] registration whose bind (at
    /// `build()`) gathers the per-port ring regions, `fsw_create`s the state,
    /// and produces a [`DlSlot`](crate::dl) instead of a typed `CyclicRunner`.
    /// Its output buffers land in the [`Registry`] like a static system's.
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
        let mut desc = loaded.descriptor().clone();
        // Dl systems are cyclic-only: the registered kind is pinned here,
        // never trusted from the decoded wire mirror.
        desc.kind = SystemKind::Cyclic;
        self.push_system(
            desc,
            name.into(),
            Reg::Dl(DlReg {
                system: loaded,
                params,
            }),
        )
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
        mut descriptor: SystemDescriptor,
        artifact: PathBuf,
        system: impl Into<String>,
        params: Vec<u8>,
    ) -> SystemHandle {
        // Cyclic-only, pinned here like the dl path: the registered kind is
        // never trusted from decoded wire bytes.
        descriptor.kind = SystemKind::Cyclic;
        self.push_system(
            descriptor,
            name.into(),
            Reg::Proc(ProcReg {
                artifact,
                params,
                system: system.into(),
            }),
        )
    }

    /// Use `exe` as the worker executable for process systems instead of
    /// re-executing the host binary. For hosts whose own binary cannot serve
    /// as a worker (or wants a leaner one).
    pub fn worker_exe(&mut self, exe: impl Into<PathBuf>) -> &mut Self {
        self.worker_exe = Some(exe.into());
        self
    }

    /// Root the run's shared-memory session directory at `dir` instead of
    /// the default (`/dev/shm` when present, else the OS temp dir).
    pub fn shm_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.shm_dir = Some(dir.into());
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
    /// list in the occupant's own order (its trailing [`SlotControlIn`] input
    /// re-marked [`PortConn::Host`] in place, since the runner holds the
    /// cancel writer), and the runner's ports are the *tail*: a declared
    /// `commands` `MsgIn<SequenceCommand>` fan-in (an ordinary edge input, so
    /// command wiring is ordinary message wiring) plus a [`PortConn::SelfTap`]
    /// view over the occupant's own [`SequenceStatus`] output on the input
    /// side; a [`SlotStatus`] output and the `"sequences"` events channel
    /// (both `Host`, registry-tapped) on the output side. [`SlotReg`] records
    /// the named port plan, so the bind arm maps the occupant `FswRing`
    /// arrays as a straight prefix walk and the occupant-side positional bind
    /// contract (and so the dl ABI) is untouched.
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
        let names: Vec<&str> = allowed.iter().map(|a| a.name.as_str()).collect();
        validate_slot_spec(&names, initial.as_ref().map(|i| i.occupant.as_str()))?;
        // Per-slot means all-occupants: the isolation boundary is the slot's
        // position in the cycle, and a mixed allow set would make `Load`
        // silently change the fault domain.
        let n_proc = allowed
            .iter()
            .filter(|a| matches!(a.backing, OccupantBacking::Artifact(_)))
            .count();
        if n_proc != 0 && n_proc != allowed.len() {
            return Err(SlotConfigError::MixedBacking);
        }
        let process = n_proc == allowed.len();
        // Every allowed occupant must share the contract; the slot sizes and
        // validates to the first occupant's descriptor (mutual subset).
        let base = &allowed[0].descriptor;
        for occ in &allowed[1..] {
            let d = &occ.descriptor;
            let ports_match = |a: &[PortDesc], b: &[PortDesc]| {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|(x, y)| compatible(x, y) && compatible(y, x))
            };
            if !(ports_match(&d.inputs, &base.inputs) && ports_match(&d.outputs, &base.outputs)) {
                return Err(SlotConfigError::OccupantMismatch {
                    occupant: occ.name.clone(),
                    base: allowed[0].name.clone(),
                });
            }
        }
        let ports = slot::SlotPorts::for_occupant(base, &allowed[0].name)?;

        let name: String = name.into();
        // The registered descriptor name is the slot's instance name (a leaked
        // `&'static str` for the descriptor field and the `SlotRunner` identity).
        let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
        let registered = ports.registered(leaked);

        Ok(self.push_system(
            registered,
            name,
            Reg::Slot(SlotReg {
                allowed,
                initial,
                ports,
                process,
            }),
        ))
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
        self.push_edge(producer, consumer, false)
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
        self.push_edge(producer, consumer, true)
    }

    fn push_edge(
        &mut self,
        producer: PortRef,
        consumer: PortRef,
        delayed: bool,
    ) -> Result<(), WireError> {
        if producer.system.id >= self.systems.len() {
            return Err(WireError::UnknownSystem {
                id: producer.system.id,
            });
        }
        if consumer.system.id >= self.systems.len() {
            return Err(WireError::UnknownSystem {
                id: consumer.system.id,
            });
        }
        if producer.port != consumer.port {
            return Err(WireError::PortIdMismatch {
                producer: producer.port,
                consumer: consumer.port,
            });
        }
        self.edges.push((producer, consumer, delayed));
        Ok(())
    }

    /// Validate the graph, size and allocate every ring, bind ports,
    /// auto-provision health/log buffers, and return a ready coordinator.
    ///
    /// One orchestrator over named passes, each handing its product to the
    /// next: validation, edge resolution, fan-out counting, ring allocation,
    /// registry freeze, copy-in planning, bind.
    pub fn build(mut self) -> Result<Coordinator, WireError> {
        // Add the wiring-manifest output to the coordinator #0 bundle before
        // any pass runs, so it is sized, allocated, registered, and bound like
        // every other port. Its ring is sized from the concrete payload — a
        // full IR overruns the default message cap — via an overridden
        // `max_size`; nothing raises the global cap.
        let wiring_manifest = self.wiring_manifest.take();
        if let Some(manifest) = &wiring_manifest {
            let mut port = PortDesc::msg_named::<WiringManifest>("wiring");
            port.conn = PortConn::Host;
            port.max_size = wiring_manifest_max_size(&manifest.ir_json);
            self.systems[0].desc.outputs.push(port);
        }
        self.validate_cycle_rate()?;
        self.validate_receive_all_last()?;
        self.validate_slot_name_caps()?;
        self.validate_port_axes()?;
        let cons_edges = self.resolve_edges()?;
        let fan_out = self.count_fan_out(&cons_edges);
        let mut alloc = self.alloc_rings(&cons_edges, &fan_out)?;
        let seq_registry = self.seq_registry_payload();
        let registry = freeze_registry(std::mem::take(&mut alloc.reg_entries))?;
        let mut plumbing = self.plan_copy_ins(&cons_edges, &mut alloc);
        let Self {
            config,
            systems,
            worker_exe,
            ..
        } = self;
        let proc_ctx = ProcBindCtx {
            step_timeout: config.proc_step_timeout,
            worker_exe,
            max_restarts: config.proc_max_restarts,
            restart_backoff: config.proc_restart_backoff,
        };
        let BoundSystems {
            cyclic,
            pending_async,
            coord,
        } = bind_systems(
            systems,
            &cons_edges,
            &alloc,
            &mut plumbing,
            &registry,
            &proc_ctx,
        )?;

        Ok(Coordinator {
            config,
            cyclic,
            pending_async,
            copy_ins: plumbing.copy_ins,
            coord_health: coord.health,
            status_out: coord.status_out,
            stopped: Vec::new(),
            stopped_scratch: Vec::new(),
            workers: Vec::new(),
            workers_scratch: Vec::new(),
            cycle: 0,
            progress: Arc::new(AtomicU64::new(0)),
            registry,
            control_out: Some(coord.control_out),
            seq_registry_out: coord.seq_registry_out,
            seq_registry,
            seq_registry_emitted: false,
            wiring_out: coord.wiring_out,
            wiring_manifest,
            wiring_emitted: false,
            reload_in: coord.reload_in,
            started: false,
            // Declared last so the canonical ring handles drop after every port.
            rings: alloc.table,
            session: alloc.session,
        })
    }

    // -----------------------------------------------------------------------
    // build() passes, in order.
    // -----------------------------------------------------------------------

    /// A Wall clock turns `cycle_rate` into the per-cycle pacing budget in
    /// `run_for`; reject an unusable rate here so the failure is a build-time
    /// `WireError`, not a `Duration::from_secs_f64` panic mid-run. A
    /// `Simulated` clock ignores the rate, so it is deliberately not validated
    /// there.
    fn validate_cycle_rate(&self) -> Result<(), WireError> {
        if matches!(self.config.clock, ClockMode::Wall)
            && !(self.config.cycle_rate.is_finite() && self.config.cycle_rate > 0.0)
        {
            return Err(WireError::InvalidCycleRate {
                rate: self.config.cycle_rate,
            });
        }
        Ok(())
    }

    /// Receive-all (telemetry) systems must register last. The downlink's
    /// end-of-cycle snapshot only observes systems stepping before it, so a
    /// cyclic system registered after it would telemeter one cycle stale.
    /// Enforced, not silently reordered: reordering registrations would change
    /// the step order the stale-edge diagnostics validate. Async systems are
    /// exempt (they run off their own task, not the registration-ordered loop).
    fn validate_receive_all_last(&self) -> Result<(), WireError> {
        let mut first_receive_all: Option<usize> = None;
        for (s, sys) in self.systems.iter().enumerate() {
            if sys.desc.kind != SystemKind::Cyclic {
                continue;
            }
            let has_receive_all = sys
                .desc
                .capabilities
                .contains(&crate::Capability::ReceiveAll);
            if has_receive_all {
                first_receive_all.get_or_insert(s);
            } else if let Some(t) = first_receive_all {
                return Err(WireError::ReceiveAllNotLast {
                    system: sys.name.clone(),
                    receive_all: self.systems[t].name.clone(),
                });
            }
        }
        Ok(())
    }

    /// Slot instance names are wire addresses: enforce the [`NAME_CAP`]. A
    /// `SequenceCommand` addresses a slot by its instance name, and the same
    /// name packs into fixed-size status frames; a longer name would telemeter
    /// truncated while addressing untruncated, so it is a build error, never a
    /// truncation.
    fn validate_slot_name_caps(&self) -> Result<(), WireError> {
        for sys in &self.systems {
            if matches!(sys.reg, Reg::Slot(_)) && sys.desc.name.len() > NAME_CAP {
                return Err(WireError::SlotNameTooLong {
                    name: sys.desc.name.to_string(),
                    len: sys.desc.name.len(),
                });
            }
        }
        Ok(())
    }

    /// Per-descriptor axis validation, needing no edges: FanIn::Many with
    /// Delivery::Snapshot is rejected (latest-wins across producers is
    /// ill-defined).
    fn validate_port_axes(&self) -> Result<(), WireError> {
        for sys in &self.systems {
            for port in &sys.desc.inputs {
                if port.fan_in == FanIn::Many && port.delivery == Delivery::Snapshot {
                    return Err(WireError::SnapshotFanIn {
                        system: sys.desc.name,
                        port: port.id,
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate every edge and build the one connection map,
    /// `(cons_id, in_idx) -> [(prod_id, out_idx)]`, covering every input: a
    /// FanIn::One input holds exactly one entry (enforced here), a FanIn::Many
    /// input zero or more. Every rule branches on a descriptor axis, never on
    /// frame-vs-message. Also runs the graph-shape checks over the map: every
    /// feedback loop must be broken by a `connect_delayed`, registration order
    /// must agree with the dataflow, and every FanIn::One input must be
    /// connected.
    fn resolve_edges(&self) -> Result<ConsEdges, WireError> {
        let n = self.systems.len();
        let mut cons_edges: ConsEdges = HashMap::new();
        // System-level adjacency over the non-delayed SNAPSHOT edges only, for
        // cycle detection: a remaining cycle is an unbroken feedback loop. Log
        // edges are excluded; a log is a decoupled event/command stream, not a
        // same-cycle dependency.
        let mut forward_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (p, c, delayed) in &self.edges {
            let prod = &self.systems[p.system.id].desc;
            let cons = &self.systems[c.system.id].desc;
            let out_idx =
                prod.outputs
                    .iter()
                    .position(|d| d.id == p.port)
                    .ok_or(WireError::UnknownPort {
                        system: p.system.id,
                        port: p.port,
                    })?;
            let in_idx =
                cons.inputs
                    .iter()
                    .position(|d| d.id == c.port)
                    .ok_or(WireError::UnknownPort {
                        system: c.system.id,
                        port: c.port,
                    })?;
            if !compatible(&prod.outputs[out_idx], &cons.inputs[in_idx]) {
                return Err(WireError::Incompatible {
                    producer: prod.name,
                    consumer: cons.name,
                    port: c.port,
                });
            }
            let in_desc = &cons.inputs[in_idx];
            // A host-connected input's counterpart is held by the system's
            // runner (the slot's cancel writer, a self-tap over its own
            // output), so an edge into it is rejected. Host *outputs* keep
            // accepting consumer edges: the coordinator's `commands` channel is
            // exactly a Host output slots read over explicit edges.
            if in_desc.conn != PortConn::Edge {
                return Err(WireError::HostPort {
                    system: cons.name,
                    port: c.port,
                });
            }
            // `delayed` marks a one-cycle-late snapshot sample; on a log input
            // it is meaningless and rejected instead of silently ignored.
            if *delayed && in_desc.delivery == Delivery::Log {
                return Err(WireError::DelayedLogEdge {
                    producer: prod.name,
                    consumer: cons.name,
                    port: c.port,
                });
            }
            let producers = cons_edges.entry((c.system.id, in_idx)).or_default();
            match in_desc.fan_in {
                // Exactly one edge per input.
                FanIn::One => {
                    if !producers.is_empty() {
                        return Err(WireError::DoubleConnect {
                            system: cons.name,
                            port: c.port,
                        });
                    }
                    producers.push((p.system.id, out_idx));
                }
                // Fan-in (append). Distinct producers may fan in freely, but an
                // exact duplicate of one edge would deliver every record twice.
                FanIn::Many => {
                    if producers.contains(&(p.system.id, out_idx)) {
                        return Err(WireError::DuplicateEdge {
                            producer: prod.name,
                            consumer: cons.name,
                            port: c.port,
                        });
                    }
                    producers.push((p.system.id, out_idx));
                }
            }
            // Self-edges included: a system plainly connected to itself is the
            // tightest feedback loop (it can only ever read its own previous
            // cycle's value), so it must be declared with `connect_delayed` like
            // any other loop; the DFS reports it as a one-member `FeedbackCycle`.
            if in_desc.delivery == Delivery::Snapshot && !delayed {
                forward_adj[p.system.id].push(c.system.id);
            }
        }

        // --- Every feedback loop must be broken by a `connect_delayed` --------
        if let Some(cycle) = find_cycle(&forward_adj) {
            return Err(WireError::FeedbackCycle {
                systems: cycle
                    .into_iter()
                    .map(|id| self.systems[id].desc.name)
                    .collect(),
            });
        }

        // --- Registration order must agree with the dataflow ------------------
        // The cyclic step loop runs in registration order, so a non-delayed
        // snapshot edge between two cyclic systems whose consumer registered
        // before its producer would read last cycle's value forever: silent
        // staleness that must instead be declared with `connect_delayed`.
        // Checked after cycle detection so a genuine unbroken loop (which
        // always contains a backward edge) reports the clearer `FeedbackCycle`.
        // Log edges are exempt (a decoupled stream); so are edges with an async
        // endpoint (async systems run off the post-step copy-in or their own
        // task, so their registration index carries no ordering semantics).
        // Self-edges never reach here (rejected above as a one-member cycle).
        for (p, c, delayed) in &self.edges {
            if *delayed {
                continue;
            }
            let prod = &self.systems[p.system.id].desc;
            let cons = &self.systems[c.system.id].desc;
            let in_delivery = cons
                .inputs
                .iter()
                .find(|d| d.id == c.port)
                .map(|d| d.delivery);
            if in_delivery != Some(Delivery::Snapshot) {
                continue;
            }
            let both_cyclic = prod.kind == SystemKind::Cyclic && cons.kind == SystemKind::Cyclic;
            if both_cyclic && c.system.id < p.system.id {
                return Err(WireError::StaleFrameEdge {
                    producer: prod.name,
                    consumer: cons.name,
                    port: c.port,
                });
            }
        }

        // --- Input coverage: a FanIn::One input must be connected exactly once ---
        // Exactly-once is the edge pass above; existence is here. A FanIn::Many
        // input may have zero producers; a non-Edge input is fed by its runner,
        // never an edge.
        for (sid, sys) in self.systems.iter().enumerate() {
            for (in_idx, port) in sys.desc.inputs.iter().enumerate() {
                if port.conn == PortConn::Edge
                    && port.fan_in == FanIn::One
                    && !cons_edges.contains_key(&(sid, in_idx))
                {
                    return Err(WireError::UnconnectedInput {
                        system: sys.desc.name,
                        port: port.id,
                    });
                }
            }
        }

        Ok(cons_edges)
    }

    /// Fan-out per output port: one uniform count over the one connection map.
    /// A declared self-tap is one more reader on the system's *own* output,
    /// counted here so the budget is explicit rather than slack-covered.
    fn count_fan_out(&self, cons_edges: &ConsEdges) -> HashMap<(usize, usize), usize> {
        let mut fan_out: HashMap<(usize, usize), usize> = HashMap::new();
        for producers in cons_edges.values() {
            for &(prod_id, out_idx) in producers {
                *fan_out.entry((prod_id, out_idx)).or_insert(0) += 1;
            }
        }
        for (sid, sys) in self.systems.iter().enumerate() {
            for port in &sys.desc.inputs {
                let PortConn::SelfTap(pid) = port.conn else {
                    continue;
                };
                let out_idx = sys
                    .desc
                    .outputs
                    .iter()
                    .position(|o| o.id == pid)
                    .expect("a SelfTap names one of the system's own outputs");
                *fan_out.entry((sid, out_idx)).or_insert(0) += 1;
            }
        }
        fan_out
    }

    /// Which output buffers cross a process boundary and must therefore be
    /// file-backed: every output of a process system, plus every output some
    /// process system consumes over an edge. A process slot crosses only its
    /// occupant *prefix* — the occupant's outputs and its Edge inputs'
    /// producers — because the runner tail (the `commands` fan-in, the
    /// self-tap, the status/events outputs) never leaves the coordinator.
    /// Everything else stays heap.
    fn shared_outputs(&self, cons_edges: &ConsEdges) -> HashSet<(usize, usize)> {
        let mut shared = HashSet::new();
        for (sid, sys) in self.systems.iter().enumerate() {
            // How much of the port lists a worker touches: all of a process
            // system's, the occupant prefix of a process slot's.
            let (n_outputs, n_inputs) = match &sys.reg {
                Reg::Proc(_) => (sys.desc.outputs.len(), sys.desc.inputs.len()),
                Reg::Slot(slot_reg) if slot_reg.process => (
                    slot_reg.ports.occupant_outputs.len(),
                    slot_reg.ports.occupant_inputs.len(),
                ),
                _ => continue,
            };
            for out_idx in 0..n_outputs {
                shared.insert((sid, out_idx));
            }
            for in_idx in 0..n_inputs {
                if let Some(producers) = cons_edges.get(&(sid, in_idx)) {
                    shared.extend(producers.iter().copied());
                }
            }
        }
        shared
    }

    /// Allocate one buffer per output port (health/log included) plus a
    /// dedicated ring per host-connected input, collecting the build-order
    /// registry entries: one list over every registered buffer, frames and
    /// message channels alike. Each receive-all capability in the graph is an
    /// extra fan-out reader on *every* buffer, so every ring's `max_readers`
    /// includes it, derived from the declared `ReceiveAll` capabilities with
    /// no per-consumer bookkeeping.
    ///
    /// A buffer in the [`shared_outputs`](Self::shared_outputs) set is
    /// allocated as an mmap ring file in the run's [`SessionDir`] (created
    /// lazily on the first one) and its path recorded for the worker
    /// manifests; the in-process handle over the same mapping is used
    /// everywhere the heap ring would have been — a ring is backing-erased,
    /// so nothing downstream can tell.
    fn alloc_rings(
        &self,
        cons_edges: &ConsEdges,
        fan_out: &HashMap<(usize, usize), usize>,
    ) -> Result<RingAlloc, WireError> {
        let depth = self.config.default_depth;
        let slack = self.config.reader_slack;
        let n_reg = self
            .systems
            .iter()
            .flat_map(|sys| sys.desc.capabilities.iter())
            .filter(|c| **c == crate::Capability::ReceiveAll)
            .count();
        let shared = self.shared_outputs(cons_edges);

        // A graph with any process system or process slot gets its session
        // directory up front: even a (pathological) portless worker still
        // needs somewhere for its control block and manifest.
        let needs_session = |reg: &Reg| {
            matches!(reg, Reg::Proc(_)) || matches!(reg, Reg::Slot(slot_reg) if slot_reg.process)
        };
        let session = if self.systems.iter().any(|s| needs_session(&s.reg)) {
            Some(
                SessionDir::create(self.shm_dir.as_deref()).map_err(|e| WireError::Shm {
                    detail: e.to_string(),
                })?,
            )
        } else {
            None
        };
        let mut alloc = RingAlloc {
            table: RingTable { rings: Vec::new() },
            output_rings: Vec::with_capacity(self.systems.len()),
            host_input_rings: HashMap::new(),
            reg_entries: Vec::new(),
            session,
            ring_paths: HashMap::new(),
            host_input_paths: HashMap::new(),
        };

        // --- One buffer per output port -----------------------------------
        for (sid, sys) in self.systems.iter().enumerate() {
            let mut row = Vec::with_capacity(sys.desc.outputs.len());
            for (out_idx, port) in sys.desc.outputs.iter().enumerate() {
                let readers = fan_out.get(&(sid, out_idx)).copied().unwrap_or(0) + n_reg + slack;
                let instance = sys.name.clone();
                let role = BufferRole::Output {
                    system: sid,
                    port: out_idx,
                };
                // One sizing path (depth by delivery, `alloc_ring`); only the
                // registry-entry shape still splits on the schema. Command
                // channels are ordinary outputs here: a slot reads a producer
                // only over an explicit edge, so the edge fan-out counts its
                // readers exactly.
                let ring = if shared.contains(&(sid, out_idx)) {
                    let session = alloc.session.as_ref().expect("proc graphs have a session");
                    let path = session.path().join(format!("{instance}.{}.ring", port.name));
                    let ring =
                        alloc_ring_at(&path, port.delivery, port.max_size, depth, readers)?;
                    alloc.ring_paths.insert((sid, out_idx), path);
                    ring
                } else {
                    alloc_ring(port.delivery, port.max_size, depth, readers)
                };
                match &port.schema {
                    PortSchema::Table { .. } => {
                        alloc
                            .reg_entries
                            .push(registry_entry(&instance, port, ring.clone()));
                        alloc.table.rings.push(RingEntry {
                            ring: ring.clone(),
                            frame_id: port
                                .id
                                .component()
                                .expect("table port keys on a ComponentId"),
                            role,
                            instance: Some(instance),
                        });
                    }
                    PortSchema::Postcard => {
                        // Registered like any buffer; the downlink taps it
                        // unless the port opted out via `telemetered = false`
                        // (a command channel, for example).
                        let entry =
                            postcard_entry(&instance, port.name, ring.clone(), port.telemetered);
                        alloc.table.rings.push(RingEntry {
                            ring: ring.clone(),
                            frame_id: entry.key,
                            role,
                            instance: Some(instance),
                        });
                        alloc.reg_entries.push(entry);
                    }
                }
                row.push(ring);
            }
            alloc.output_rings.push(row);
        }

        // --- Dedicated rings for host-connected inputs ---------------------
        // A Host input's counterpart is its runner's writer (the slot's cancel
        // frame), so it gets its own ring instead of a producer edge. The
        // occupant attaches one read `View` per Load (released on each
        // Stop/Reset/Unload), so 1 reader slot plus slack covers the reload
        // cycle. No registry entry: it is inbound control, not an output.
        // SelfTap inputs allocate nothing (they view the system's own output,
        // already counted in `fan_out`).
        for (sid, sys) in self.systems.iter().enumerate() {
            for (in_idx, port) in sys.desc.inputs.iter().enumerate() {
                if port.conn != PortConn::Host {
                    continue;
                }
                // A process slot's control ring is the one Host-connected
                // input that crosses outward: the writer stays host-side (the
                // runner's cancel `Output`) while the occupant's read `View`
                // attaches in the worker, so it is file-backed like a crossing
                // output, path recorded for the worker manifests.
                let ring = if matches!(&sys.reg, Reg::Slot(slot_reg) if slot_reg.process) {
                    let session = alloc.session.as_ref().expect("proc graphs have a session");
                    let path = session.path().join(format!("{}.{}.ring", sys.name, port.name));
                    let ring =
                        alloc_ring_at(&path, port.delivery, port.max_size, depth, 1 + slack)?;
                    alloc.host_input_paths.insert((sid, in_idx), path);
                    ring
                } else {
                    alloc_ring(port.delivery, port.max_size, depth, 1 + slack)
                };
                alloc.table.rings.push(RingEntry {
                    ring: ring.clone(),
                    frame_id: port
                        .id
                        .component()
                        .expect("v1 host-connected inputs are table ports"),
                    role: BufferRole::Private {
                        system: sid,
                        input: in_idx,
                    },
                    instance: Some(sys.name.clone()),
                });
                alloc.host_input_rings.insert((sid, in_idx), ring);
            }
        }

        Ok(alloc)
    }

    /// The boot `SequenceRegistry` payload: one spec per slot, keyed by the
    /// slot's instance name, the channel's wire address. There is no
    /// build-order channel id.
    fn seq_registry_payload(&self) -> SequenceRegistry {
        let channels = self
            .systems
            .iter()
            .filter_map(|sys| match &sys.reg {
                Reg::Slot(slot_reg) => Some(SequenceChannelSpec {
                    name: sys.desc.name.to_string(),
                    available: slot_reg.allowed.iter().map(|a| a.name.clone()).collect(),
                }),
                _ => None,
            })
            .collect();
        SequenceRegistry { channels }
    }

    /// Private copy-in buffers for async inputs, keyed on the delivery axis.
    /// An async system cannot be step-gated, so an async snapshot input is
    /// decoupled through a private latest-wins copy-in ring, which also
    /// supplies the matched data `Notifier` the async `recv` parks on. Log
    /// inputs use a direct fan-in multi-view, an every-record log the consumer
    /// poll-drains, with no copy-in.
    fn plan_copy_ins(&self, cons_edges: &ConsEdges, alloc: &mut RingAlloc) -> AsyncPlumbing {
        let depth = self.config.default_depth;
        let slack = self.config.reader_slack;
        let mut plumbing = AsyncPlumbing {
            private_inputs: HashMap::new(),
            async_wakes: vec![Vec::new(); self.systems.len()],
            copy_ins: Vec::new(),
        };
        for (sid, sys) in self.systems.iter().enumerate() {
            if sys.desc.kind != SystemKind::Async {
                continue;
            }
            for (in_idx, port) in sys.desc.inputs.iter().enumerate() {
                // Only edge-connected snapshot inputs are copy-in decoupled; a
                // Host/SelfTap input is fed by its runner, not a producer edge.
                if port.delivery == Delivery::Log || port.conn != PortConn::Edge {
                    continue;
                }
                let (prod_id, out_idx) = cons_edges[&(sid, in_idx)][0];
                let private = alloc_ring(port.delivery, port.max_size, depth, 1 + slack);
                let data = Notifier::default();
                // The matched DATA notifier wakes the parked async `recv`; the
                // copy-in uses `try_write` and skips a full private ring. Each
                // private copy-in ring is created here and gets this one
                // writer, so the claim is always free.
                let writer = private
                    .writer(data.clone())
                    .expect("private copy-in ring has exactly one writer");
                let upstream = alloc.output_rings[prod_id][out_idx]
                    .view(NoWake)
                    .expect("producer reader slot reserved at sizing time");
                plumbing.copy_ins.push(CopyIn {
                    upstream,
                    writer,
                    last_committed: u64::MAX,
                });
                plumbing
                    .private_inputs
                    .insert((sid, in_idx), (private.clone(), data.clone()));
                plumbing.async_wakes[sid].push(data);
                alloc.table.rings.push(RingEntry {
                    ring: private,
                    frame_id: port.id.component().expect("copy-in inputs are table ports"),
                    role: BufferRole::Private {
                        system: sid,
                        input: in_idx,
                    },
                    instance: Some(sys.name.clone()),
                });
            }
        }
        plumbing
    }
}

// ---------------------------------------------------------------------------
// build() products
// ---------------------------------------------------------------------------

/// The one connection map, `(consumer, input-index)` to the producer endpoints
/// explicitly wired into it. Product of [`CoordinatorBuilder::resolve_edges`];
/// consumed by fan-out counting, copy-in planning, and the bind pass.
type ConsEdges = HashMap<(usize, usize), Vec<(usize, usize)>>;

/// The ring-allocation pass product: the canonical owning [`RingTable`], one
/// buffer row per system's outputs, the dedicated host-input rings, and the
/// build-order registry entries (drained by [`freeze_registry`]).
struct RingAlloc {
    table: RingTable,
    output_rings: Vec<Vec<RingBuffer>>,
    host_input_rings: HashMap<(usize, usize), RingBuffer>,
    reg_entries: Vec<RegistryEntry>,
    /// The run's shared-memory session, created lazily by the first
    /// file-backed ring; `None` for a graph with no process systems. Moves
    /// into the [`Coordinator`], which owns the directory's lifetime.
    session: Option<SessionDir>,
    /// The ring file behind each file-backed output buffer, for the worker
    /// manifests (`(system, out_idx)` → path).
    ring_paths: HashMap<(usize, usize), PathBuf>,
    /// The ring file behind each file-backed host-connected input (a process
    /// slot's control ring), for the worker manifests, keyed like
    /// `host_input_rings`.
    host_input_paths: HashMap<(usize, usize), PathBuf>,
}

/// The copy-in planning product: each async snapshot input's private ring plus
/// matched data notifier, the per-system wake lists (for teardown), and the
/// copy-in jobs.
struct AsyncPlumbing {
    private_inputs: HashMap<(usize, usize), (RingBuffer, Notifier)>,
    async_wakes: Vec<Vec<Notifier>>,
    copy_ins: Vec<CopyIn>,
}

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

/// Freeze the one registry every consumer's bind pulls. Frames and channels
/// share one keyspace, so a same-instance name collision between a frame and a
/// channel (both `"<instance>.<name>"`) is detectable instead of shadowing.
fn freeze_registry(reg_entries: Vec<RegistryEntry>) -> Result<Arc<Registry>, WireError> {
    let mut seen_keys: HashMap<ComponentId, usize> = HashMap::new();
    for (i, e) in reg_entries.iter().enumerate() {
        if seen_keys.insert(e.key, i).is_some() {
            return Err(WireError::DuplicateRegistryKey {
                key: format!("{}.{}", e.instance, e.name),
            });
        }
    }
    Ok(Arc::new(Registry::new(reg_entries)))
}

/// Find any directed cycle in the system graph (over the non-delayed edges),
/// returning its members in loop order, or `None` if the graph is acyclic. A
/// plain depth-first search colouring nodes white/grey/black; a back-edge to a
/// grey (on-stack) node closes a cycle, reconstructed from the DFS stack.
fn find_cycle(adj: &[Vec<usize>]) -> Option<Vec<usize>> {
    const WHITE: u8 = 0;
    const GREY: u8 = 1;
    const BLACK: u8 = 2;
    let n = adj.len();
    let mut color = vec![WHITE; n];
    let mut stack: Vec<usize> = Vec::new();

    fn dfs(
        u: usize,
        adj: &[Vec<usize>],
        color: &mut [u8],
        stack: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        color[u] = GREY;
        stack.push(u);
        for &v in &adj[u] {
            match color[v] {
                GREY => {
                    // Back-edge: the cycle is the stack tail from `v` onward.
                    let start = stack.iter().position(|&x| x == v).unwrap_or(0);
                    return Some(stack[start..].to_vec());
                }
                WHITE => {
                    if let Some(c) = dfs(v, adj, color, stack) {
                        return Some(c);
                    }
                }
                _ => {}
            }
        }
        stack.pop();
        color[u] = BLACK;
        None
    }

    for s in 0..n {
        if color[s] == WHITE
            && let Some(c) = dfs(s, adj, &mut color, &mut stack)
        {
            return Some(c);
        }
    }
    None
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

/// The one ring-sizing helper. A snapshot port is sized at the configured
/// default depth (a latest-wins sample needs little history), a log port at
/// [`LOG_DEPTH`] (an every-record stream must absorb a slow tap).
fn alloc_ring(
    delivery: Delivery,
    max_size: usize,
    default_depth: usize,
    max_readers: usize,
) -> RingBuffer {
    let depth = match delivery {
        Delivery::Snapshot => default_depth,
        Delivery::Log => LOG_DEPTH,
    };
    RingBuffer::create_in_memory(Config {
        capacity: capacity_for(max_size, depth),
        max_readers,
    })
}

/// The mmap sibling of [`alloc_ring`]: identical sizing, but the region is a
/// file in the run's session directory, attachable by a worker process. An
/// I/O failure is a build-time [`WireError::Shm`].
fn alloc_ring_at(
    path: &std::path::Path,
    delivery: Delivery,
    max_size: usize,
    default_depth: usize,
    max_readers: usize,
) -> Result<RingBuffer, WireError> {
    let depth = match delivery {
        Delivery::Snapshot => default_depth,
        Delivery::Log => LOG_DEPTH,
    };
    RingBuffer::create_mmap(
        path,
        Config {
            capacity: capacity_for(max_size, depth),
            max_readers,
        },
    )
    .map_err(|e| WireError::Shm {
        detail: format!("ring `{}`: {e}", path.display()),
    })
}

/// Mint the single [`MsgOut`] writer over a coordinator-owned ring, the
/// [`slot_writer`] analogue for a message channel. Called exactly once per
/// ring at build; the region's writer claim enforces it.
fn owned_writer<M: Msg>(ring: &RingBuffer) -> MsgOut<M> {
    // Each coordinator-owned message ring gets its single writer minted
    // exactly once at build, so the claim is always free here.
    let writer = ring
        .writer(NoWake)
        .expect("coordinator message ring has exactly one writer");
    MsgOut::new(writer)
}

/// Worst-case record bytes for a [`WiringManifest`] carrying `ir_json`, the
/// `max_size` the coordinator sizes the `wiring` ring from. A record is the
/// 2-byte [`Msg::ID`] plus the postcard body: the `u32` `ir_version` (≤5-byte
/// varint), the JSON's length prefix (≤5-byte varint), and the JSON bytes.
/// Rounded up to a 1 KiB boundary for headroom, with the default message cap
/// as a floor so a small mission's ring is no smaller than an ordinary one.
fn wiring_manifest_max_size(ir_json: &str) -> usize {
    (ir_json.len() + 12)
        .next_multiple_of(1024)
        .max(MAX_MSG_BYTES)
}

/// Build a postcard [`RegistryEntry`] for one message channel: the
/// instance-qualified key `ComponentId::new("<instance>.<name>")` (the on-wire
/// identity) over a clone of the ring, the [`registry_entry`] sibling for the
/// self-describing record. No vtable or announce; the record's 2-byte id is
/// the schema.
fn postcard_entry(
    instance: &str,
    name: &str,
    ring: RingBuffer,
    telemetered: bool,
) -> RegistryEntry {
    RegistryEntry {
        key: ComponentId::new(&format!("{instance}.{name}")),
        instance: Arc::from(instance),
        name: Arc::from(name),
        schema: EntrySchema::Postcard,
        delivery: Delivery::Log,
        telemetered,
        ring,
    }
}

/// The synthetic instance prefix coordinator-owned buffers register under:
/// they have no system instance, so their qualified key is
/// `coordinator.health` / `coordinator.log` / `coordinator.coordinator_status`.
const COORDINATOR_INSTANCE: &str = "coordinator";

/// Build a [`RegistryEntry`] for one buffer: compute the instance-qualified
/// key and the prefixed announce vtable and metadata once, capturing a clone
/// of the ring as the read source.
fn registry_entry(instance: &str, port: &PortDesc, ring: RingBuffer) -> RegistryEntry {
    let key = ComponentId::new(&format!("{instance}.{}", port.name));
    // Only table ports come through here (the caller branches on the schema),
    // so the checked accessors are always `Some`. `announce` is an
    // `Arc<dyn Fn>` (not directly callable); deref to a `&dyn Fn`.
    let announce = port
        .announce()
        .expect("table port carries an announce factory");
    let (vtable, metadata) = (**announce)(instance);
    RegistryEntry {
        key,
        instance: Arc::from(instance),
        name: Arc::from(port.name),
        schema: EntrySchema::Table {
            frame_id: port
                .id
                .component()
                .expect("table port keys on a ComponentId"),
            vtable,
            metadata,
        },
        delivery: port.delivery,
        telemetered: port.telemetered,
        ring,
    }
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
                    name: slot.name(),
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
                let name = self.stopped[i].name;
                self.coord_health.error("system_stopped");
                self.coord_health.log(Level::Warn, name);
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
                    let (name, len) = pack_name(sys.name);
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
                    let (name, len) = pack_name(w.name);
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
