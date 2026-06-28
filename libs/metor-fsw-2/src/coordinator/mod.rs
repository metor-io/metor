//! Work-Package 5 — the coordinator (coordinator.md).
//!
//! The coordinator owns the ring regions, wires the system graph at build time,
//! drives cyclic systems once per cycle, spawns async systems, folds in the async
//! copy-in step, and provisions per-system health. It is built in two phases: a
//! [`CoordinatorBuilder`] registers systems and edges and `build()`s a ready
//! [`Coordinator`] (validating, sizing, allocating rings, binding ports); then
//! [`Coordinator::run_for`] drives the run phase on `stellarator`.
//!
//! Almost everything below the builder is reuse (the ring, the typed ports, the
//! descriptors, the health port, the grown [`CyclicRunner`](crate::CyclicRunner)).
//! The genuinely new surface is the builder + validation/sizing pass, the
//! `bind`/[`Binder`] contract (see `binder.rs`), the lapped → hard-stop slot, the
//! copy-in jobs, and the lifecycle driver.

use core::mem::offset_of;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicUsize,
    Ordering::{Acquire, Release},
};
use std::time::{Duration, Instant};

use metor_fsw_ring::{
    BoxBacking, Config, NoWake, Notifier, Overrun, RingBuffer, View, WakeSource, Writer,
};
use metor_proto::types::{ComponentId, Timestamp};
use stellarator::sync::WaitQueue;
use stellarator::{JoinHandle, JoinHandleDropGuard};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::binder::{BindPorts, Binder, BoundPort};
use crate::descriptor::{Hz, PortDesc, SystemDescriptor, SystemKind};
use crate::dynamic::FrameList;
use crate::health::{HealthPort, Level};
use crate::port::{Input, Output, buffer_capacity, capacity_for};
use crate::registry::{OutputRegistry, RegistryEntry};
use crate::system::{AsyncSystem, CyclicRunner, CyclicSystem, Out, System, SystemOutput};
use crate::telemetry::{TelemetryConfig, TelemetrySystem, Transport};
use crate::{DEFAULT_DEPTH, Frame};
use crate::descriptor::compatible;

/// Slack reader slots added on top of a buffer's fan-out, covering late taps such
/// as a db/telemetry sink or a debugger (v1 has no crash-slot reclamation, so
/// `max_readers` must be set at build time — coordinator.md §2.3, Q7).
const READER_SLACK: usize = 4;

/// Bounded window a teardown gives async tasks to exit cooperatively before their
/// `drop_guard` cancels them.
const JOIN_TIMEOUT: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// Public configuration / addressing / errors
// ---------------------------------------------------------------------------

/// Which clock drives the per-cycle timestamp (and whether the loop paces itself).
#[derive(Clone, Copy, Debug)]
pub enum ClockMode {
    /// Wall-clock time: each cycle's `now` is `Timestamp::now()`, and the loop
    /// sleeps to hold `cycle_rate` (run-fast-then-wait). The default.
    Wall,
    /// A simulated clock: each cycle's `now` advances by `dt` from a start epoch and
    /// the loop does **not** sleep — it runs cycles as fast as possible. `dt` is the
    /// logical step (e.g. a physics integrator's), so a mission converges in fixed
    /// sim time regardless of how fast the host runs it (coordinator.md §6).
    Simulated { dt: Duration },
}

impl Default for ClockMode {
    fn default() -> Self {
        ClockMode::Wall
    }
}

/// Coordinator-wide configuration (coordinator.md §1.1).
#[derive(Clone, Copy, Debug)]
pub struct CoordinatorConfig {
    /// The single global cycle rate the loop holds (run-fast-then-wait) under a
    /// [`Wall`](ClockMode::Wall) clock. Every cyclic system runs every cycle; there
    /// is no per-system rate division (v1). Ignored under a `Simulated` clock.
    pub cycle_rate: Hz,
    /// In-flight record depth for a buffer whose `PortDesc` carries no rate hint.
    pub default_depth: usize,
    /// The clock driving the per-cycle `now` and loop pacing (default `Wall`).
    pub clock: ClockMode,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            cycle_rate: 100.0,
            default_depth: DEFAULT_DEPTH,
            clock: ClockMode::Wall,
        }
    }
}

/// A handle to a registered system, returned by `add_cyclic`/`add_async` and used
/// to address its ports in [`PortRef`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SystemHandle {
    id: usize,
}

/// Addresses one port as `(system, frame_id)` — both come straight off the
/// already-derived `SystemDescriptor`, so WP6 can resolve a KDL edge to a `connect`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PortRef {
    pub system: SystemHandle,
    pub frame_id: ComponentId,
}

impl PortRef {
    /// Address the port carrying frame `F` on `system`.
    pub fn new<F: Frame>(system: SystemHandle) -> Self {
        Self {
            system,
            frame_id: F::FRAME_ID,
        }
    }
}

/// A wiring error caught at build time, before any byte flows (coordinator.md §2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// A `PortRef` named a system index that was never registered.
    UnknownSystem { id: usize },
    /// A system has no port carrying the named frame.
    UnknownPort { system: usize, frame_id: ComponentId },
    /// `connect` named a producer and consumer port that do not share a frame id.
    FrameIdMismatch {
        producer: ComponentId,
        consumer: ComponentId,
    },
    /// The producer's frame does not satisfy the consumer's required shape.
    Incompatible {
        producer: &'static str,
        consumer: &'static str,
        frame_id: ComponentId,
    },
    /// An input port was never connected (nothing would ever write it).
    UnconnectedInput {
        system: &'static str,
        frame_id: ComponentId,
    },
    /// Two producers were connected into one input port.
    DoubleConnect {
        system: &'static str,
        frame_id: ComponentId,
    },
    /// A feedback loop was left unbroken: a cycle remains in the graph once the
    /// intentional one-cycle-delayed edges (`connect_delayed`) are removed. Every
    /// feedback loop must break exactly one of its edges with `connect_delayed`, so
    /// that the one-cycle-late sampling is explicit rather than an artifact of
    /// registration order. `systems` names the cycle members in loop order.
    FeedbackCycle { systems: Vec<&'static str> },
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::UnknownSystem { id } => write!(f, "unknown system handle #{id}"),
            WireError::UnknownPort { system, frame_id } => {
                write!(f, "system #{system} has no port for frame {frame_id:?}")
            }
            WireError::FrameIdMismatch { producer, consumer } => write!(
                f,
                "connect frame-id mismatch: producer {producer:?} vs consumer {consumer:?}"
            ),
            WireError::Incompatible {
                producer,
                consumer,
                frame_id,
            } => write!(
                f,
                "incompatible edge {producer} -> {consumer} on frame {frame_id:?}"
            ),
            WireError::UnconnectedInput { system, frame_id } => {
                write!(f, "{system} input for frame {frame_id:?} is not connected")
            }
            WireError::DoubleConnect { system, frame_id } => write!(
                f,
                "{system} input for frame {frame_id:?} connected more than once"
            ),
            WireError::FeedbackCycle { systems } => write!(
                f,
                "unbroken feedback cycle {} — break one edge with connect_delayed",
                systems.join(" -> ")
            ),
        }
    }
}

impl std::error::Error for WireError {}

// ---------------------------------------------------------------------------
// Slot state (the lapped → permanent hard-stop, coordinator.md §3.3/§3.4)
// ---------------------------------------------------------------------------

/// Why a cyclic slot hard-stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopReason {
    /// An input was lapped (its data was overwritten before the system read it).
    LappedInput,
    /// A dlopen'd system panicked inside the `.so` (the boundary caught it and
    /// returned [`FswStatus::Panicked`](crate::abi::FswStatus); dl-open.md §2.5).
    /// Only reachable for a [`DlSlot`](crate::dl); a static `CyclicRunner` cannot
    /// produce it (a panic there unwinds the host directly).
    Panicked,
}

impl StopReason {
    fn code(self) -> u8 {
        match self {
            StopReason::LappedInput => 1,
            StopReason::Panicked => 2,
        }
    }
}

/// A cyclic slot's lifecycle state. Once `Stopped` it is never cleared in v1
/// (recovery is future work).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotState {
    Running,
    Stopped { reason: StopReason },
}

impl SlotState {
    pub fn is_stopped(&self) -> bool {
        matches!(self, SlotState::Stopped { .. })
    }
}

/// One stopped cyclic system, surfaced through [`Coordinator::stopped`] and the
/// coordinator status frame.
#[derive(Clone, Copy, Debug)]
pub struct StoppedSystem {
    pub name: &'static str,
    pub reason: StopReason,
}

/// The grown per-system slot trait object the coordinator drives (coordinator.md
/// §3.4); implemented for [`CyclicRunner`](crate::CyclicRunner) in `system.rs`.
pub(crate) trait CyclicSlot {
    fn init(&mut self);
    fn step(&mut self, now: Timestamp);
    fn shutdown(&mut self);
    fn name(&self) -> &'static str;
    fn state(&self) -> &SlotState;
}

// ---------------------------------------------------------------------------
// Coordinator status frame (coordinator.md §3.3/§5.3)
// ---------------------------------------------------------------------------

/// Capacity of one stopped-system name in the status frame (longer truncated).
pub const STATUS_NAME_CAP: usize = 48;
/// Max stopped systems named in one status record.
pub const MAX_STOPPED: usize = 32;

/// One stopped-system entry in [`CoordinatorStatus`]: a reason code, a used name
/// length, and a fixed-size name buffer.
#[derive(crate::AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
struct StoppedEntry {
    reason: u8,
    len: u8,
    _pad: [u8; 6],
    name: [u8; STATUS_NAME_CAP],
}

/// The coordinator's own status frame (NAME = `"coordinator"`): which cyclic
/// systems have hard-stopped and why, in addition to each system's own health.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "coordinator_status")]
struct CoordinatorStatus {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    cycle: u64,
    stopped_count: u64,
    stopped: FrameList<StoppedEntry, MAX_STOPPED>,
}

// ---------------------------------------------------------------------------
// Ring registry (coordinator.md §1.2)
// ---------------------------------------------------------------------------

/// What a buffer carries — for diagnostics and to make ownership explicit. The
/// fields are kept for diagnostics/debugging even though nothing reads them yet.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
enum BufferRole {
    Output { system: usize, port: usize },
    Private { system: usize, input: usize },
    Coordinator,
}

/// One owned ring plus its identity. `ring` is the canonical handle whose sole job
/// is to outlive every port over it (the ports hold their own `Arc` clones).
#[allow(dead_code)]
struct RingEntry {
    ring: RingBuffer<BoxBacking>,
    frame_id: ComponentId,
    role: BufferRole,
    /// The KDL/builder instance name of the system owning this buffer (the
    /// telemetry-sink prefix, wiring.md §6). `None` for coordinator-owned buffers.
    instance: Option<String>,
}

/// Owns every `RingBuffer<BoxBacking>`. Holding the canonical handle here keeps a
/// buffer alive longer than any port over it, regardless of teardown order.
struct RingTable {
    rings: Vec<RingEntry>,
}

// ---------------------------------------------------------------------------
// Async plumbing
// ---------------------------------------------------------------------------

/// A private-buffer copy-in job (coordinator.md §4.2/§4.3): drains an upstream
/// producer's output and mirrors it into the async system's private buffer. The
/// private buffer is `Overwrite`, so `try_write` never blocks and silently
/// overwrites unconsumed records when the async consumer is behind (drop-on-full).
struct CopyIn {
    upstream: View<BoxBacking, NoWake, NoWake>,
    writer: Writer<BoxBacking, Notifier, Notifier>,
    scratch: Vec<u8>,
}

/// Per-task signals the coordinator hands a spawned async system: a stop flag, an
/// init-readiness barrier, and a go-gate that holds the first `run` pass until
/// every system's `init` has completed (coordinator.md init-barrier decision).
struct LaunchCtx {
    stop: Arc<AtomicBool>,
    ready: Arc<WaitQueue>,
    ready_count: Arc<AtomicUsize>,
    go: Arc<WaitQueue>,
    go_flag: Arc<AtomicBool>,
}

/// A bound async system ready to be spawned once. Erased so the coordinator can
/// hold a heterogeneous set.
trait AsyncLauncher {
    fn launch(self: Box<Self>, ctx: LaunchCtx) -> JoinHandle<()>;
}

/// A bound async system: its `run` future borrows all three for the loop, so they
/// move into the spawned task (coordinator.md §4.1).
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

/// A spawned async task plus the handles the coordinator drives its lifecycle with.
/// The `drop_guard` cancels the task if it does not exit cooperatively (and when a
/// `Coordinator` is dropped mid-run).
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
    O: SystemOutput + BindPorts<BoxBacking> + 'static,
    S::Input: BindPorts<BoxBacking> + 'static,
{
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn CyclicSlot> {
        // The host always binds over `BoxBacking`; the `Binder` is the host's
        // `RingSource<B = BoxBacking>` (dl-open.md §1.2). A dlopen'd system instead
        // monomorphizes `CyclicRunner<_, _, RawBacking>` on its own side of the ABI.
        let input = <S::Input as BindPorts<BoxBacking>>::bind(binder);
        let output = <Out<O> as BindPorts<BoxBacking>>::bind(binder);
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
    S::Input: BindPorts<BoxBacking> + 'static,
    S::Output: BindPorts<BoxBacking> + 'static,
{
    fn bind(self: Box<Self>, binder: &mut Binder) -> Box<dyn AsyncLauncher> {
        let input = <S::Input as BindPorts<BoxBacking>>::bind(binder);
        let output = <S::Output as BindPorts<BoxBacking>>::bind(binder);
        Box::new(AsyncSlot {
            system: self.system,
            input,
            output,
        })
    }
}

/// A registered dlopen'd cyclic system (dl-open.md §4.3): the loaded handle plus its
/// postcard `Params` blob. At `build()` it is turned into a [`DlSlot`](crate::dl)
/// instead of a typed [`CyclicRunner`]; everything before that (descriptor push, edge
/// validation, ring sizing/allocation, registry entry) is the static-system path,
/// unchanged. Available without `kdl` (Wave 3a — dl-open.md §3.0).
struct DlReg {
    system: crate::dl::DlSystem,
    params: Vec<u8>,
}

enum Reg {
    Cyclic(Box<dyn CyclicRegistration>),
    Async(Box<dyn AsyncRegistration>),
    /// A dlopen'd cyclic system, bound to a [`DlSlot`](crate::dl) at `build()`.
    Dl(DlReg),
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Registers systems and edges, then `build`s a ready [`Coordinator`]. This is
/// WP6's target surface (coordinator.md §2.1).
pub struct CoordinatorBuilder {
    config: CoordinatorConfig,
    regs: Vec<Reg>,
    descs: Vec<SystemDescriptor>,
    kinds: Vec<SystemKind>,
    /// Per-system instance name (defaults to `System::NAME`; the wiring loader
    /// supplies a distinct KDL instance name — wiring.md §6). Parallel to `descs`.
    names: Vec<String>,
    /// Each registered edge `(producer, consumer, delayed)`. A `delayed` edge is an
    /// intentional one-cycle-delayed feedback edge, excluded from cycle detection.
    edges: Vec<(PortRef, PortRef, bool)>,
    /// How many systems pull the broad output registry (telemetry.md §2.5). Each one
    /// is an extra fan-out consumer on *every* output buffer, so `build()` sizes every
    /// ring's `max_readers` to include it. Bumped by [`add_telemetry`](Self::add_telemetry).
    n_registry_consumers: usize,
}

impl CoordinatorBuilder {
    fn new(config: CoordinatorConfig) -> Self {
        Self {
            config,
            regs: Vec::new(),
            descs: Vec::new(),
            kinds: Vec::new(),
            names: Vec::new(),
            edges: Vec::new(),
            n_registry_consumers: 0,
        }
    }

    /// Register a cyclic system under its type's `System::NAME` instance name; see
    /// [`add_cyclic_named`](Self::add_cyclic_named) to name the instance explicitly.
    pub fn add_cyclic<S, O>(&mut self, system: S) -> SystemHandle
    where
        S: CyclicSystem<Output = Out<O>> + 'static,
        O: SystemOutput + BindPorts<BoxBacking> + 'static,
        S::Input: BindPorts<BoxBacking> + 'static,
    {
        self.add_cyclic_named(<S as System>::NAME, system)
    }

    /// Register a cyclic system under an explicit instance name; returns a handle
    /// whose ports can be `connect`ed. The instance name disambiguates two instances
    /// of one system type at the telemetry sink (wiring.md §6).
    pub fn add_cyclic_named<S, O>(&mut self, name: impl Into<String>, system: S) -> SystemHandle
    where
        S: CyclicSystem<Output = Out<O>> + 'static,
        O: SystemOutput + BindPorts<BoxBacking> + 'static,
        S::Input: BindPorts<BoxBacking> + 'static,
    {
        let id = self.descs.len();
        self.descs.push(<S as CyclicSystem>::descriptor());
        self.kinds.push(SystemKind::Cyclic);
        self.names.push(name.into());
        self.regs.push(Reg::Cyclic(Box::new(CyclicReg { system })));
        SystemHandle { id }
    }

    /// Register an async system under its type's `System::NAME` instance name.
    pub fn add_async<S>(&mut self, system: S) -> SystemHandle
    where
        S: AsyncSystem + 'static,
        S::Input: BindPorts<BoxBacking> + 'static,
        S::Output: BindPorts<BoxBacking> + 'static,
    {
        self.add_async_named(<S as System>::NAME, system)
    }

    /// Register an async system under an explicit instance name (wiring.md §6).
    pub fn add_async_named<S>(&mut self, name: impl Into<String>, system: S) -> SystemHandle
    where
        S: AsyncSystem + 'static,
        S::Input: BindPorts<BoxBacking> + 'static,
        S::Output: BindPorts<BoxBacking> + 'static,
    {
        let id = self.descs.len();
        self.descs.push(<S as AsyncSystem>::descriptor());
        self.kinds.push(SystemKind::Async);
        self.names.push(name.into());
        self.regs.push(Reg::Async(Box::new(AsyncReg { system })));
        SystemHandle { id }
    }

    /// Register the telemetry downlink (telemetry.md §8). This is an ordinary
    /// [`add_cyclic_named`](Self::add_cyclic_named) of a [`TelemetrySystem`] under the
    /// instance name `"telemetry"`, registered **last** so its end-of-cycle snapshot
    /// observes every other system's fresh output. It additionally records one registry
    /// consumer, so `build()` reserves a reader slot for it on every output buffer.
    pub fn add_telemetry<T>(&mut self, config: TelemetryConfig<T>) -> SystemHandle
    where
        T: Transport + 'static,
    {
        self.n_registry_consumers += 1;
        self.add_cyclic_named("telemetry", TelemetrySystem::new(config))
    }

    /// Register a dlopen'd cyclic system under an explicit instance name (dl-open.md
    /// §4.3). `loaded` is an opened [`DlSystem`](crate::dl); `params` is the canonical
    /// postcard `Params` blob the `.so` decodes in `fsw_create` (identical on the wire
    /// from either front-end — dl-open.md §6.3).
    ///
    /// This is the dl twin of [`add_cyclic_named`](Self::add_cyclic_named): it pushes
    /// the `.so`'s reconstructed [`SystemDescriptor`] so the **existing**
    /// `compatible()`/`WireError` validation and ring sizing/allocation run over it
    /// unchanged, and records a [`Reg::Dl`] registration whose `bind` (at `build()`)
    /// gathers the per-port ring regions, `fsw_create`s the state, and produces a
    /// [`DlSlot`](crate::dl) instead of a typed `CyclicRunner`. Its output buffers land
    /// in the [`OutputRegistry`] with the (prefixed) announce, so telemetry `All` taps
    /// them like a static system's.
    ///
    /// v1 is cyclic-only (dl-open.md §3.1). This is the low-level builder method; the
    /// Wave 3a [`resolve`](crate::resolve) drives it from a [`Wiring`](crate::Wiring)
    /// (built in Rust or parsed from the KDL `artifact`/`lib=` surface).
    pub fn add_dl_cyclic(
        &mut self,
        name: impl Into<String>,
        loaded: crate::dl::DlSystem,
        params: Vec<u8>,
    ) -> SystemHandle {
        let id = self.descs.len();
        self.descs.push(loaded.descriptor().clone());
        self.kinds.push(SystemKind::Cyclic);
        self.names.push(name.into());
        self.regs.push(Reg::Dl(DlReg {
            system: loaded,
            params,
        }));
        SystemHandle { id }
    }

    /// Connect a producer output to a consumer input, addressed by frame id. The
    /// full compatibility/structural validation runs in [`build`](Self::build);
    /// this only catches the cheap frame-id and unknown-system/port mistakes early.
    ///
    /// A forward (acyclic) edge. If a `connect` happens to close a feedback loop in
    /// registration order, `build` rejects it as a [`FeedbackCycle`](WireError::FeedbackCycle):
    /// the back-edge of a loop must be declared with [`connect_delayed`](Self::connect_delayed).
    pub fn connect(&mut self, producer: PortRef, consumer: PortRef) -> Result<(), WireError> {
        self.push_edge(producer, consumer, false)
    }

    /// Connect a producer to a consumer marking the edge as an intentional
    /// one-cycle-**delayed** feedback edge (the back-edge of a control loop). The
    /// runtime path is identical to [`connect`](Self::connect) — a `view()` read of
    /// the latest committed value, which is last cycle's because the producer runs
    /// after the consumer in registration order — but the edge is excluded from
    /// cycle detection, so the loop builds. Every feedback loop must break exactly
    /// one edge this way; an unbroken cycle is a [`FeedbackCycle`](WireError::FeedbackCycle).
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
        if producer.system.id >= self.descs.len() {
            return Err(WireError::UnknownSystem {
                id: producer.system.id,
            });
        }
        if consumer.system.id >= self.descs.len() {
            return Err(WireError::UnknownSystem {
                id: consumer.system.id,
            });
        }
        if producer.frame_id != consumer.frame_id {
            return Err(WireError::FrameIdMismatch {
                producer: producer.frame_id,
                consumer: consumer.frame_id,
            });
        }
        self.edges.push((producer, consumer, delayed));
        Ok(())
    }

    /// Validate the graph, size and allocate every ring, bind ports, auto-provision
    /// health/log buffers, and return a ready coordinator (coordinator.md §2).
    pub fn build(mut self) -> Result<Coordinator, WireError> {
        let n = self.descs.len();
        let depth = self.config.default_depth;

        // --- Validate edges, build the connection map ------------------------
        // (cons_id, in_idx) -> (prod_id, out_idx)
        let mut cons_edge: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
        // System-level adjacency over the NON-delayed edges only, for cycle
        // detection: a remaining cycle is an unbroken feedback loop.
        let mut forward_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (p, c, delayed) in &self.edges {
            let out_idx = self.descs[p.system.id]
                .outputs
                .iter()
                .position(|d| d.frame_id == p.frame_id)
                .ok_or(WireError::UnknownPort {
                    system: p.system.id,
                    frame_id: p.frame_id,
                })?;
            let in_idx = self.descs[c.system.id]
                .inputs
                .iter()
                .position(|d| d.frame_id == c.frame_id)
                .ok_or(WireError::UnknownPort {
                    system: c.system.id,
                    frame_id: c.frame_id,
                })?;
            if !compatible(
                &self.descs[p.system.id].outputs[out_idx],
                &self.descs[c.system.id].inputs[in_idx],
            ) {
                return Err(WireError::Incompatible {
                    producer: self.descs[p.system.id].name,
                    consumer: self.descs[c.system.id].name,
                    frame_id: c.frame_id,
                });
            }
            if cons_edge
                .insert((c.system.id, in_idx), (p.system.id, out_idx))
                .is_some()
            {
                return Err(WireError::DoubleConnect {
                    system: self.descs[c.system.id].name,
                    frame_id: c.frame_id,
                });
            }
            if !delayed && p.system.id != c.system.id {
                forward_adj[p.system.id].push(c.system.id);
            }
        }

        // --- Every feedback loop must be broken by a `connect_delayed` --------
        if let Some(cycle) = find_cycle(&forward_adj) {
            return Err(WireError::FeedbackCycle {
                systems: cycle.into_iter().map(|id| self.descs[id].name).collect(),
            });
        }

        // --- Every input must be connected exactly once ----------------------
        for s in 0..n {
            for (in_idx, port) in self.descs[s].inputs.iter().enumerate() {
                if !cons_edge.contains_key(&(s, in_idx)) {
                    return Err(WireError::UnconnectedInput {
                        system: self.descs[s].name,
                        frame_id: port.frame_id,
                    });
                }
            }
        }

        // --- Fan-out per output port -----------------------------------------
        let mut fan_out: HashMap<(usize, usize), usize> = HashMap::new();
        for &(prod_id, out_idx) in cons_edge.values() {
            *fan_out.entry((prod_id, out_idx)).or_insert(0) += 1;
        }

        let mut table = RingTable { rings: Vec::new() };
        // The build-order registry entries, one per tappable output buffer. Collected
        // alongside allocation and frozen into an `Arc<OutputRegistry>` *before* the
        // bind loop, so a system can pull it in `BindPorts::bind` (telemetry.md §2.3).
        let mut reg_entries: Vec<RegistryEntry> = Vec::new();
        // Each registry consumer is an extra fan-out reader on every output buffer.
        let n_reg = self.n_registry_consumers;

        // --- Allocate one buffer per output port (incl. health/log) ----------
        let mut output_rings: Vec<Vec<RingBuffer<BoxBacking>>> = Vec::with_capacity(n);
        for s in 0..n {
            let mut row = Vec::with_capacity(self.descs[s].outputs.len());
            for (out_idx, port) in self.descs[s].outputs.iter().enumerate() {
                let readers = fan_out.get(&(s, out_idx)).copied().unwrap_or(0) + n_reg + READER_SLACK;
                let ring = RingBuffer::create_in_memory(Config {
                    capacity: capacity_for(port.max_size, depth),
                    max_readers: readers,
                    overrun: Overrun::Overwrite,
                });
                row.push(ring.clone());
                let instance = self.names[s].clone();
                reg_entries.push(registry_entry(&instance, port, ring.clone()));
                table.rings.push(RingEntry {
                    ring,
                    frame_id: port.frame_id,
                    role: BufferRole::Output {
                        system: s,
                        port: out_idx,
                    },
                    instance: Some(instance),
                });
            }
            output_rings.push(row);
        }

        // --- Coordinator's own health / log / status buffers (telemetry.md §2.3) ---
        // Allocated *before* the bind loop (they depend on no edges) so the registry
        // handed to systems includes them. Sized for the coordinator-side reader
        // (status_view) plus the registry consumers.
        let coord_readers = 1 + n_reg + READER_SLACK;
        let health_ring = coord_ring::<crate::SystemHealth>(depth, coord_readers);
        let log_ring = coord_ring::<crate::SystemLog>(depth, coord_readers);
        let status_ring = coord_ring::<CoordinatorStatus>(depth, coord_readers);
        for (ring, desc) in [
            (health_ring.clone(), PortDesc::of::<crate::SystemHealth>()),
            (log_ring.clone(), PortDesc::of::<crate::SystemLog>()),
            (status_ring.clone(), PortDesc::of::<CoordinatorStatus>()),
        ] {
            reg_entries.push(registry_entry(COORDINATOR_INSTANCE, &desc, ring.clone()));
            table.rings.push(RingEntry {
                ring,
                frame_id: desc.frame_id,
                role: BufferRole::Coordinator,
                instance: None,
            });
        }

        // Freeze the registry; every consumer's bind pulls this same handle.
        let registry = Arc::new(OutputRegistry::new(reg_entries));

        // --- Private copy-in buffers for async inputs ------------------------
        // (async_sys, in_idx) -> (private ring, matched data notifier, space notifier)
        let mut private_inputs: HashMap<(usize, usize), (RingBuffer<BoxBacking>, Notifier, Notifier)> =
            HashMap::new();
        let mut async_wakes: Vec<Vec<Notifier>> = vec![Vec::new(); n];
        let mut copy_ins: Vec<CopyIn> = Vec::new();
        for s in 0..n {
            if self.kinds[s] != SystemKind::Async {
                continue;
            }
            for in_idx in 0..self.descs[s].inputs.len() {
                let (prod_id, out_idx) = cons_edge[&(s, in_idx)];
                let port = &self.descs[s].inputs[in_idx];
                let private = RingBuffer::create_in_memory(Config {
                    capacity: capacity_for(port.max_size, depth),
                    max_readers: 1 + READER_SLACK,
                    overrun: Overrun::Overwrite,
                });
                let data = Notifier::default();
                let space = Notifier::default();
                let writer = private.writer(data.clone(), space.clone());
                let upstream = output_rings[prod_id][out_idx]
                    .view(NoWake, NoWake)
                    .expect("producer reader slot reserved at sizing time");
                copy_ins.push(CopyIn {
                    upstream,
                    writer,
                    scratch: Vec::new(),
                });
                private_inputs.insert((s, in_idx), (private.clone(), data.clone(), space.clone()));
                async_wakes[s].push(data);
                table.rings.push(RingEntry {
                    ring: private,
                    frame_id: port.frame_id,
                    role: BufferRole::Private {
                        system: s,
                        input: in_idx,
                    },
                    instance: Some(self.names[s].clone()),
                });
            }
        }

        // --- Bind every system's ports over the allocated rings --------------
        let mut cyclic: Vec<Box<dyn CyclicSlot>> = Vec::new();
        let mut pending_async: Vec<PendingAsync> = Vec::new();
        let regs = std::mem::take(&mut self.regs);
        for (id, reg) in regs.into_iter().enumerate() {
            match reg {
                // A dlopen'd system binds over **raw** `FswRing` regions, not typed
                // `BoundPort`s: gather the same per-port rings the coordinator allocated
                // (outputs = this system's own buffers; inputs = views into the upstream
                // producers' outputs — the cyclic-consumer path), as `(base, len, role)`
                // handles in `descriptors()` order, and hand them to a `DlSlot`
                // (dl-open.md §4.2). Sizing, allocation, validation, and the registry
                // entry above are identical to a static system's.
                Reg::Dl(dl) => {
                    use crate::abi::{FswRing, ROLE_INPUT, ROLE_OUTPUT};
                    let outputs: Vec<FswRing> = (0..self.descs[id].outputs.len())
                        .map(|out_idx| {
                            let (base, len) = output_rings[id][out_idx].region();
                            FswRing { base, len, role: ROLE_OUTPUT }
                        })
                        .collect();
                    let inputs: Vec<FswRing> = (0..self.descs[id].inputs.len())
                        .map(|in_idx| {
                            let (prod_id, out_idx) = cons_edge[&(id, in_idx)];
                            let (base, len) = output_rings[prod_id][out_idx].region();
                            FswRing { base, len, role: ROLE_INPUT }
                        })
                        .collect();
                    // SAFETY: every region named here is a `RingTable`-owned ring that
                    // outlives the slot — the coordinator drops `cyclic` (this slot,
                    // whose `Drop` calls `fsw_destroy`) before `rings` (dl-open.md §2.5).
                    let slot = unsafe {
                        dl.system
                            .into_slot(&dl.params, inputs, outputs, self.descs[id].name)
                    };
                    cyclic.push(Box::new(slot));
                }
                // The static (host-side `BoxBacking`) path: build typed `BoundPort`s and
                // walk them with a `Binder`.
                reg => {
                    // Outputs: default wakes, the system's own buffers.
                    let outs: Vec<BoundPort> = (0..self.descs[id].outputs.len())
                        .map(|out_idx| BoundPort::new(output_rings[id][out_idx].clone()))
                        .collect();
                    // Inputs: cyclic consumers view the producer's output directly; async
                    // consumers view their private copy-in buffer with the matched wake.
                    let ins: Vec<BoundPort> = (0..self.descs[id].inputs.len())
                        .map(|in_idx| {
                            let (prod_id, out_idx) = cons_edge[&(id, in_idx)];
                            if self.kinds[id] == SystemKind::Async {
                                let (ring, data, space) = private_inputs[&(id, in_idx)].clone();
                                BoundPort::matched(ring, Box::new(data), Box::new(space))
                            } else {
                                BoundPort::new(output_rings[prod_id][out_idx].clone())
                            }
                        })
                        .collect();

                    let mut binder = Binder::new(&outs, &ins, registry.clone());
                    match reg {
                        Reg::Cyclic(r) => cyclic.push(r.bind(&mut binder)),
                        Reg::Async(r) => pending_async.push(PendingAsync {
                            launcher: r.bind(&mut binder),
                            wake_on_stop: async_wakes[id].clone(),
                        }),
                        // The dl arm is handled by the outer match.
                        Reg::Dl(_) => unreachable!("dl registration bound by the outer match"),
                    }
                }
            }
        }

        // --- Coordinator's own health / log / status ports -------------------
        // The buffers were allocated up front (above) so the registry includes them;
        // here we just wrap the writer/view ports over those same ring handles.
        let coord_health = HealthPort::new(
            Output::new(health_ring.writer(NoWake, NoWake)),
            Output::new(log_ring.writer(NoWake, NoWake)),
        );
        let status_out = Output::new(status_ring.writer(NoWake, NoWake));
        let status_view =
            Input::new(status_ring.view(NoWake, NoWake).expect("status reader slot"));

        Ok(Coordinator {
            config: self.config,
            cyclic,
            pending_async,
            copy_ins,
            coord_health,
            status_out,
            status_view,
            stopped: Vec::new(),
            cycle: 0,
            registry,
            // Declared last so the canonical ring handles drop after every port.
            rings: table,
        })
    }
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

    fn dfs(u: usize, adj: &[Vec<usize>], color: &mut [u8], stack: &mut Vec<usize>) -> Option<Vec<usize>> {
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
        if color[s] == WHITE {
            if let Some(c) = dfs(s, adj, &mut color, &mut stack) {
                return Some(c);
            }
        }
    }
    None
}

fn coord_ring<F: Frame>(depth: usize, max_readers: usize) -> RingBuffer<BoxBacking> {
    RingBuffer::create_in_memory(Config {
        capacity: buffer_capacity::<F>(depth),
        max_readers,
        overrun: Overrun::Overwrite,
    })
}

/// The synthetic instance prefix coordinator-owned buffers downlink under
/// (telemetry.md §6): they have no system instance, so their qualified key is
/// `coordinator.health` / `coordinator.log` / `coordinator.coordinator_status`.
const COORDINATOR_INSTANCE: &str = "coordinator";

/// Build a [`RegistryEntry`] for one buffer: compute the instance-qualified key and
/// the prefixed announce vtable+metadata once (telemetry.md §2.1/§6), capturing a
/// clone of the ring as the read source.
fn registry_entry(instance: &str, port: &PortDesc, ring: RingBuffer<BoxBacking>) -> RegistryEntry {
    let key = ComponentId::new(&format!("{instance}.{}", port.frame_name));
    // `announce` is an `Arc<dyn Fn>` (not directly callable); deref to a `&dyn Fn`.
    let (vtable, metadata) = (*port.announce)(instance);
    RegistryEntry {
        key,
        instance: Arc::from(instance),
        frame_id: port.frame_id,
        vtable,
        metadata,
        ring,
    }
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

/// The wired, ready flight-software graph. Drives cyclic systems once per cycle,
/// runs the async copy-in step, spawns and tears down async systems, and emits
/// coordinator-level health + a status frame.
pub struct Coordinator {
    config: CoordinatorConfig,
    cyclic: Vec<Box<dyn CyclicSlot>>,
    pending_async: Vec<PendingAsync>,
    copy_ins: Vec<CopyIn>,
    coord_health: HealthPort,
    status_out: Output<CoordinatorStatus>,
    status_view: Input<CoordinatorStatus>,
    stopped: Vec<StoppedSystem>,
    cycle: u64,
    /// The broad output registry over every tappable buffer (telemetry.md §2).
    registry: Arc<OutputRegistry>,
    /// Canonical ring handles; declared last so they drop after every port.
    #[allow(dead_code)]
    rings: RingTable,
}

impl Coordinator {
    /// Start a builder.
    pub fn builder(config: CoordinatorConfig) -> CoordinatorBuilder {
        CoordinatorBuilder::new(config)
    }

    /// The cyclic systems that have hard-stopped (lapped input), as also published
    /// in the coordinator status frame.
    pub fn stopped(&self) -> &[StoppedSystem] {
        &self.stopped
    }

    /// The broad output registry over every tappable buffer (telemetry.md §2): an
    /// index a logger, recorder, debugger, or test can use to read any output by its
    /// instance-qualified id `ComponentId::new("<instance>.<frame>")`.
    pub fn registry(&self) -> Arc<OutputRegistry> {
        self.registry.clone()
    }

    /// Every owned **output** buffer as `(instance-name, frame-id)`. The instance
    /// name is the unique per-system handle a telemetry sink prefixes records with
    /// (`<instance>.<frame>.<component>`), so two instances of one system type emit
    /// distinct fully-qualified paths despite sharing a `frame_id` (wiring.md §6).
    pub fn output_instances(&self) -> Vec<(&str, ComponentId)> {
        self.rings
            .rings
            .iter()
            .filter(|e| matches!(e.role, BufferRole::Output { .. }))
            .filter_map(|e| e.instance.as_deref().map(|name| (name, e.frame_id)))
            .collect()
    }

    /// Read the latest coordinator status frame back (the stopped systems and
    /// their reason codes), for telemetry/test inspection.
    pub fn read_status(&mut self) -> Option<Vec<(String, u8)>> {
        let rec = self.status_view.latest().ok().flatten()?;
        let list = rec.list::<StoppedEntry>(offset_of!(CoordinatorStatus, stopped));
        let mut out = Vec::new();
        for e in list.iter() {
            let n = (e.len as usize).min(STATUS_NAME_CAP);
            let name = String::from_utf8_lossy(&e.name[..n]).into_owned();
            out.push((name, e.reason));
        }
        Some(out)
    }

    /// Run the lifecycle for a bounded number of cycles: init all (barrier) → run
    /// → shutdown all. Convenient for tests and bounded missions.
    pub async fn run_for(&mut self, cycles: usize) {
        let tasks = self.start().await;
        let budget = Duration::from_secs_f64(1.0 / self.config.cycle_rate);
        // The epoch a `Simulated` clock advances from; unused under `Wall`.
        let epoch = Timestamp::now();
        for k in 0..cycles {
            let start = Instant::now();
            self.cycle += 1;
            // The per-cycle timestamp every system shares: wall time, or the
            // simulated clock at `epoch + k*dt` (coordinator.md §6, fix #5/#6).
            let now = match self.config.clock {
                ClockMode::Wall => Timestamp::now(),
                ClockMode::Simulated { dt } => epoch + dt * k as u32,
            };
            for slot in &mut self.cyclic {
                slot.step(now);
            }
            self.run_copy_ins();
            self.update_status(now);
            match self.config.clock {
                // Wall: hold the cycle rate (run-fast-then-wait).
                ClockMode::Wall => {
                    let elapsed = start.elapsed();
                    if elapsed < budget {
                        stellarator::sleep(budget - elapsed).await;
                    } else {
                        self.telemeter_overrun(now, elapsed, budget);
                    }
                }
                // Simulated: no pacing — run as fast as possible. Still yield once so
                // any spawned async consumer (driven by the copy-in above) gets to run
                // on this cooperative runtime.
                ClockMode::Simulated { .. } => stellarator::yield_now().await,
            }
        }
        self.shutdown(tasks).await;
    }

    /// Phase 1: spawn async systems (each inits + signals readiness), wait for the
    /// init barrier, run cyclic inits, then release the async tasks. Holds the
    /// barrier so every `init` completes before the first cycle or any `run` pass.
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

    /// Mirror fresh upstream records into each async system's private buffer
    /// (drop-on-full via overwrite), waking the async `recv` (coordinator.md §4.3).
    fn run_copy_ins(&mut self) {
        for c in &mut self.copy_ins {
            loop {
                match c.upstream.try_read_into(&mut c.scratch) {
                    Ok(true) => {
                        let _ = c.writer.try_write(&c.scratch);
                    }
                    Ok(false) => break,
                    Err(_) => {
                        // Copy-in lapped on the upstream output: skip to the live
                        // edge and continue (the async consumer gets latest-wins).
                        c.upstream.resync();
                        break;
                    }
                }
            }
        }
    }

    /// Scan the slots; when the stopped set changes, refresh the status frame and
    /// log the change to coordinator health.
    fn update_status(&mut self, now: Timestamp) {
        let mut cur: Vec<StoppedSystem> = Vec::new();
        for slot in &self.cyclic {
            if let SlotState::Stopped { reason } = slot.state() {
                cur.push(StoppedSystem {
                    name: slot.name(),
                    reason: *reason,
                });
            }
        }
        if cur.len() == self.stopped.len() {
            return;
        }
        self.stopped = cur;
        self.publish_status(now);
        let names: Vec<&'static str> = self.stopped.iter().map(|s| s.name).collect();
        for name in names {
            self.coord_health.error("system_stopped");
            self.coord_health.log(Level::Warn, name);
        }
        self.coord_health.end_cycle(now, 0);
    }

    fn publish_status(&mut self, now: Timestamp) {
        let entries: Vec<(u8, &'static str)> = self
            .stopped
            .iter()
            .map(|s| (s.reason.code(), s.name))
            .collect();
        let frame = CoordinatorStatus {
            timestamp: now,
            cycle: self.cycle,
            stopped_count: entries.len() as u64,
            stopped: FrameList::EMPTY,
        };
        let _ = self.status_out.write_with(&frame, |fw| {
            fw.list(offset_of!(CoordinatorStatus, stopped), |l| {
                for (reason, name) in &entries {
                    let bytes = name.as_bytes();
                    let len = bytes.len().min(STATUS_NAME_CAP);
                    let mut buf = [0u8; STATUS_NAME_CAP];
                    buf[..len].copy_from_slice(&bytes[..len]);
                    l.push(StoppedEntry {
                        reason: *reason,
                        len: len as u8,
                        _pad: [0; 6],
                        name: buf,
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

    /// Cooperative teardown (coordinator.md §6): signal every async task, wake any
    /// parked `recv`, give the tasks a brief window to finish their current pass
    /// and run their own `shutdown`, then drop the tasks — whose `drop_guard`
    /// cancels any still parked (the non-cooperative timeout path). Finally
    /// `shutdown` the cyclic systems in reverse registration order. The
    /// `RingTable` drops last (struct field order).
    async fn shutdown(&mut self, tasks: Vec<AsyncTask>) {
        for t in &tasks {
            t.stop.store(true, Release);
            for n in &t.wake_on_stop {
                n.notify();
            }
        }
        // A task parked in `Input::recv` cannot be woken without data (the wait
        // re-checks for a committed record), so a recv-driven loop only exits on
        // the next datum; the bounded window lets timer- and data-paced tasks
        // observe `stop` and flush in `System::shutdown` before we cancel.
        stellarator::sleep(JOIN_TIMEOUT).await;
        drop(tasks);
        for slot in self.cyclic.iter_mut().rev() {
            slot.shutdown();
        }
    }
}

#[cfg(test)]
mod tests;
