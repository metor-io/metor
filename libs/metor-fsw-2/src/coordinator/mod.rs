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
    AtomicBool, AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::time::{Duration, Instant};

use metor_fsw_ring::{
    BoxBacking, Config, NoWake, Notifier, Overrun, RingBuffer, View, WakeSource, Writer,
};
use metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceChannelSpec, SequenceCommand, SequenceRegistry,
};
use stellarator::sync::WaitQueue;
use stellarator::{JoinHandle, JoinHandleDropGuard};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::binder::{BindPorts, Binder, BoundInput, BoundPort};
use crate::descriptor::compatible;
use crate::descriptor::{
    Delivery, FanIn, Hz, OnLap, PortConn, PortDesc, PortId, PortSchema, SystemDescriptor,
    SystemKind,
};
use crate::dynamic::FrameList;
use crate::health::{HealthPort, Level, SystemHealth, SystemLog};
use crate::message::{LOG_DEPTH, MsgIn, MsgOut};
use crate::port::{Input, Output, capacity_for};
use crate::registry::{EntrySchema, Registry, RegistryEntry};
use crate::sequence::{SequenceStatus, SlotControlIn};
use crate::system::{AsyncSystem, CyclicRunner, CyclicSystem, Out, System, SystemOutput};
use crate::telemetry::{RecvTransport, TelemetryConfig, TelemetrySystem, Transport, UplinkSystem};
use crate::{DEFAULT_DEPTH, Frame};

mod slot;
pub use slot::{AllowedOccupant, InitialOccupant, SLOT_NAME_CAP, SlotStatus};
use slot::{SlotReg, SlotRunner, slot_writer};

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

/// Addresses one port as `(system, port)` — both come straight off the already-derived
/// `SystemDescriptor`, so the wiring loader can resolve a KDL edge to a `connect`.
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

    /// Address the port carrying message `M` on `system` (`docs/message-wiring.md` §3).
    pub fn msg<M: Msg>(system: SystemHandle) -> Self {
        Self {
            system,
            port: PortId::Packet(M::ID),
        }
    }
}

/// A wiring error caught at build time, before any byte flows (coordinator.md §2.2).
///
/// Not `Eq`: [`InvalidCycleRate`](WireError::InvalidCycleRate) carries the offending
/// `f64` rate so the message can name it.
#[derive(Clone, Debug, PartialEq)]
pub enum WireError {
    /// A `PortRef` named a system index that was never registered.
    UnknownSystem { id: usize },
    /// A system has no port carrying the named frame or message.
    UnknownPort {
        system: usize,
        port: PortId,
    },
    /// `connect` named a producer and consumer port that do not share a port id.
    PortIdMismatch {
        producer: PortId,
        consumer: PortId,
    },
    /// The producer's record shape does not satisfy the consumer's required shape
    /// (the Table subset rule / Postcard id equality / delivery agreement).
    Incompatible {
        producer: &'static str,
        consumer: &'static str,
        port: PortId,
    },
    /// A [`FanIn::One`](crate::FanIn) input port was never connected (nothing would
    /// ever write it). [`FanIn::Many`](crate::FanIn) inputs may be unconnected (zero
    /// producers is legal, `docs/message-wiring.md` §3.2).
    UnconnectedInput {
        system: &'static str,
        port: PortId,
    },
    /// Two producers were connected into one [`FanIn::One`](crate::FanIn) input port.
    /// [`FanIn::Many`](crate::FanIn) inputs allow fan-in, so this never fires for
    /// them — but an *exact duplicate* of one edge is a
    /// [`DuplicateEdge`](Self::DuplicateEdge).
    DoubleConnect {
        system: &'static str,
        port: PortId,
    },
    /// The exact same fan-in edge — one `(producer, consumer, port)` triple — was
    /// connected twice. Fan-in of *distinct* producers is legal (`docs/message-wiring.md`
    /// §3.2); a copy-pasted duplicate edge would deliver every record to the consumer
    /// twice (a double-applied command), so it is rejected.
    DuplicateEdge {
        producer: &'static str,
        consumer: &'static str,
        port: PortId,
    },
    /// `connect_delayed` (or KDL `delayed=#true`) on an edge into a
    /// [`Delivery::Log`](crate::Delivery) input (A7). `delayed` marks a one-cycle-late
    /// *snapshot* sample; a log is a decoupled event/command stream with no same-cycle
    /// dependency, so the delay is meaningless — rejected instead of silently ignored.
    DelayedLogEdge {
        producer: &'static str,
        consumer: &'static str,
        port: PortId,
    },
    /// An **async** system declared a [`Delivery::Log`](crate::Delivery) input with
    /// [`OnLap::Stop`](crate::OnLap). An async system cannot be step-gated, so the
    /// framework has no way to honor a hard-stop-on-lap doctrine there: its Log
    /// inputs poll-drain the shared producer rings directly. (A Snapshot input's
    /// default Stop is instead the documented coercion — effectively Resync,
    /// implemented by the drop-on-full copy-in ring.)
    StopOnAsyncInput {
        system: &'static str,
        port: PortId,
    },
    /// An input declared [`FanIn::Many`](crate::FanIn) with
    /// [`Delivery::Snapshot`](crate::Delivery): latest-wins across several producers
    /// is ill-defined without cross-ring ordering, so the combination is rejected.
    SnapshotFanIn {
        system: &'static str,
        port: PortId,
    },
    /// An edge targets a **host-connected** input (`PortConn::Host`/`SelfTap`, A3):
    /// its counterpart is held by the system's runner (the slot runner's cancel
    /// writer, a self-tap view over the system's own output), never an edge — a slot
    /// occupant's `slot_control` is written by `Abort`, not by another system.
    HostPort {
        system: &'static str,
        port: PortId,
    },
    /// A non-delayed **Snapshot** edge points *backward* in registration order between
    /// two cyclic systems: the cyclic step loop runs in registration order, so the
    /// consumer would execute before its producer every cycle and permanently read the
    /// previous cycle's value — the exact staleness
    /// [`connect_delayed`](CoordinatorBuilder::connect_delayed) exists to make
    /// explicit. Fix by registering the producer before the consumer, or declare the
    /// one-cycle delay with `connect_delayed`. Log edges are exempt (a decoupled
    /// stream, not a same-cycle dependency), as are edges touching an async endpoint
    /// (async systems run off the copy-in step / their own task, not the
    /// registration-ordered step loop).
    StaleFrameEdge {
        producer: &'static str,
        consumer: &'static str,
        port: PortId,
    },
    /// The configured `cycle_rate` cannot pace a [`Wall`](ClockMode::Wall) clock: it must
    /// be finite and positive to become a per-cycle `Duration` budget (a 0/negative/NaN/
    /// infinite rate would panic in `Duration::from_secs_f64` at run time). A
    /// [`Simulated`](ClockMode::Simulated) clock ignores the rate, so it is not
    /// validated there.
    InvalidCycleRate { rate: Hz },
    /// A feedback loop was left unbroken: a cycle remains in the graph once the
    /// intentional one-cycle-delayed edges (`connect_delayed`) are removed. Every
    /// feedback loop must break exactly one of its edges with `connect_delayed`, so
    /// that the one-cycle-late sampling is explicit rather than an artifact of
    /// registration order. `systems` names the cycle members in loop order.
    FeedbackCycle { systems: Vec<&'static str> },
    /// Two registered buffers computed the same instance-qualified registry key
    /// `"<instance>.<name>"` — one keyspace over frames and channels makes the
    /// collision detectable instead of silently shadowing one entry (A1/C3).
    DuplicateRegistryKey { key: String },
    /// A slot instance name exceeds [`NAME_CAP`] bytes. Slot names are the sequence
    /// channels' **wire address** (`SequenceCommand::channel`) *and* must round-trip
    /// losslessly into the fixed-size frames that carry them (`SlotStatus::occupant`,
    /// the coordinator status entries) — a longer name would telemeter truncated while
    /// addressing untruncated, so it is rejected at build instead of silently truncated.
    SlotNameTooLong { name: String, len: usize },
    /// A cyclic system **without** a receive-all port was registered after one **with**
    /// it (the telemetry downlink). The downlink's end-of-cycle snapshot only observes
    /// systems that step *before* it, so a later registration would telemeter one cycle
    /// stale — enforced, not silently reordered (reordering would change the step order
    /// the stale-edge diagnostics validate). Fix: register `system` before the
    /// receive-all system. Async systems are exempt (they are not in the step order).
    /// Both fields are **instance** names.
    ReceiveAllNotLast { system: String, receive_all: String },
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::UnknownSystem { id } => write!(f, "unknown system handle #{id}"),
            WireError::UnknownPort { system, port } => {
                write!(f, "system #{system} has no port {port:?}")
            }
            WireError::PortIdMismatch { producer, consumer } => write!(
                f,
                "connect port-id mismatch: producer {producer:?} vs consumer {consumer:?}"
            ),
            WireError::Incompatible {
                producer,
                consumer,
                port,
            } => write!(
                f,
                "incompatible edge {producer} -> {consumer} on port {port:?}"
            ),
            WireError::UnconnectedInput { system, port } => {
                write!(f, "{system} input for port {port:?} is not connected")
            }
            WireError::DoubleConnect { system, port } => write!(
                f,
                "{system} input for port {port:?} connected more than once"
            ),
            WireError::DuplicateEdge {
                producer,
                consumer,
                port,
            } => write!(
                f,
                "duplicate edge {producer} -> {consumer} on port {port:?} — \
                 the same edge was connected twice (every record would deliver twice)"
            ),
            WireError::DelayedLogEdge {
                producer,
                consumer,
                port,
            } => write!(
                f,
                "delayed edge {producer} -> {consumer} on Log port {port:?}: `delayed` \
                 marks a one-cycle-late snapshot sample, which is meaningless on a \
                 decoupled event/command log — drop the delayed flag"
            ),
            WireError::StopOnAsyncInput { system, port } => write!(
                f,
                "async system {system} declares OnLap::Stop on input {port:?}: an async \
                 consumer cannot be step-gated, so the framework cannot honor a \
                 hard-stop-on-lap policy there — use OnLap::Resync"
            ),
            WireError::SnapshotFanIn { system, port } => write!(
                f,
                "{system} input {port:?} declares FanIn::Many with Delivery::Snapshot: \
                 latest-wins across several producers is ill-defined — use Delivery::Log \
                 or FanIn::One"
            ),
            WireError::HostPort { system, port } => write!(
                f,
                "{system} input {port:?} is host-connected: its counterpart is held by \
                 the system's runner, not an edge — remove the edge"
            ),
            WireError::StaleFrameEdge {
                producer,
                consumer,
                port,
            } => write!(
                f,
                "{consumer} is registered before {producer} but consumes its {port:?} \
                 output: it would step first every cycle and permanently read the \
                 previous cycle's value — register {producer} before {consumer}, or \
                 declare the one-cycle delay with connect_delayed"
            ),
            WireError::InvalidCycleRate { rate } => write!(
                f,
                "cycle_rate {rate} cannot pace a Wall clock — it must be finite and positive"
            ),
            WireError::DuplicateRegistryKey { key } => write!(
                f,
                "two buffers share the registry key {key:?} — rename one instance or port \
                 so every '<instance>.<name>' is unique"
            ),
            WireError::SlotNameTooLong { name, len } => write!(
                f,
                "slot instance name {name:?} is {len} bytes; the sequence-channel wire \
                 address is capped at {NAME_CAP} bytes"
            ),
            WireError::ReceiveAllNotLast {
                system,
                receive_all,
            } => write!(
                f,
                "cyclic system '{system}' is registered after the receive-all system \
                 '{receive_all}' (the telemetry downlink), whose end-of-cycle snapshot \
                 would miss it; register '{system}' before the telemetry downlink"
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
    /// returned [`FswStatus::Panicked`](crate::abi::FswStatus)).
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

/// A cyclic slot's lifecycle state — the **one** lifecycle enum every slot kind shares.
/// A static [`CyclicRunner`](crate::CyclicRunner) and a build-time
/// [`DlSlot`](crate::dl::DlSlot) only ever inhabit `Running`/`Stopped` (once `Stopped`
/// they are never cleared in v1); the runtime [`SlotRunner`](slot) uses all five, and a
/// runtime slot recovers from a terminal state via `Load`/`Reset`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotState {
    /// No occupant; `step` is a cheap no-op. (Runtime slots only.)
    Empty,
    /// An occupant is created and bound (its future built) but not yet polling. After a
    /// hard-drop `Stop` the state returns to `Loaded` with no live future. (Runtime
    /// slots only.)
    Loaded,
    /// The slot is polled every cycle.
    Running,
    /// The occupant's future returned `Ready` — terminal **success**, not an error-stop.
    /// The `Completed`/`Aborted`/`Failed` detail rides the occupant's own
    /// [`SequenceStatus`](crate::sequence::SequenceStatus) frame; `outcome` is its
    /// latched `run_state` byte. (Runtime slots only.)
    Done { outcome: u8 },
    /// Hard-stopped: an input was lapped or the `.so` panicked.
    Stopped { reason: StopReason },
}

impl SlotState {
    /// The projection the coordinator's stopped-systems status uses: only a
    /// lapped/panicked stop is an error-stop (`Done`/`Empty`/`Loaded` are not).
    pub fn stop_reason(&self) -> Option<StopReason> {
        match self {
            SlotState::Stopped { reason } => Some(*reason),
            _ => None,
        }
    }

    /// The wire phase code published in [`SlotStatus::phase`]
    /// (Empty=0/Loaded=1/Running=2/Done=3/Stopped=4).
    pub fn code(&self) -> u8 {
        match self {
            SlotState::Empty => 0,
            SlotState::Loaded => 1,
            SlotState::Running => 2,
            SlotState::Done { .. } => 3,
            SlotState::Stopped { .. } => 4,
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stop_reason().is_some()
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

/// The one shared byte cap on every name packed into a fixed-size host frame (a stopped
/// system in [`CoordinatorStatus`], the occupant in [`SlotStatus`]) — and the validated
/// cap on slot instance names, which double as the sequence channels' **wire address**
/// (`SequenceCommand::channel`). Matches
/// [`SEQUENCE_CHANNEL_NAME_CAP`](metor_proto_wkt::SEQUENCE_CHANNEL_NAME_CAP).
pub const NAME_CAP: usize = 48;

// The build-validated cap and the wire protocol's documented cap are one invariant.
const _: () = assert!(NAME_CAP == metor_proto_wkt::SEQUENCE_CHANNEL_NAME_CAP);

/// Capacity of one stopped-system name in the status frame (longer truncated).
pub const STATUS_NAME_CAP: usize = NAME_CAP;

/// Pack a name into a fixed [`NAME_CAP`] buffer + used length (truncating) — the one
/// packing helper for every fixed-size name field in the host frames.
pub(crate) fn pack_name(name: &str) -> ([u8; NAME_CAP], u8) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(NAME_CAP);
    let mut buf = [0u8; NAME_CAP];
    buf[..n].copy_from_slice(&bytes[..n]);
    (buf, n as u8)
}
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
    /// A system's declared output buffer (the coordinator's own #0 outputs
    /// included — A9 removed the special `Coordinator` role).
    Output { system: usize, port: usize },
    /// A dedicated input-side ring: an async copy-in buffer, or a Host-connected
    /// input's ring (A3).
    Private { system: usize, input: usize },
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
    /// The private ring's sole writer: the matched data `Notifier` wakes the parked
    /// async `recv`; the space side is `NoWake` — an Overwrite write never suspends.
    writer: Writer<BoxBacking, Notifier, NoWake>,
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
        // `RingSource<B = BoxBacking>`. A dlopen'd system instead monomorphizes
        // `CyclicRunner<_, _, RawBacking>` on its own side of the ABI.
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

/// A registered dlopen'd cyclic system: the loaded handle plus its postcard `Params`
/// blob. At `build()` it is turned into a [`DlSlot`](crate::dl) instead of a typed
/// [`CyclicRunner`]; everything before that (descriptor push, edge validation, ring
/// sizing/allocation, registry entry) is the same as the static-system path.
/// Available without `kdl`.
struct DlReg {
    system: crate::dl::DlSystem,
    params: Vec<u8>,
}

enum Reg {
    Cyclic(Box<dyn CyclicRegistration>),
    Async(Box<dyn AsyncRegistration>),
    /// A dlopen'd cyclic system, bound to a [`DlSlot`](crate::dl) at `build()`.
    Dl(DlReg),
    /// A runtime-swappable slot, bound to a [`SlotRunner`](slot::SlotRunner) at `build()`.
    Slot(SlotReg),
    /// The coordinator itself, registered as system #0 under the reserved instance
    /// name `"coordinator"` (`docs/design-command-slots.md` §2.6). A **marker**
    /// registration: its declared outputs are allocated/registered by the uniform
    /// passes like any system's, but it is never pushed into `cyclic` (the
    /// coordinator *is* the loop) — the bind arm wraps the allocated rings into the
    /// coordinator's own fields instead.
    Coordinator,
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Registers systems and edges, then `build`s a ready [`Coordinator`]. This is the
/// surface the wiring loader targets (coordinator.md §2.1).
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
}

impl CoordinatorBuilder {
    fn new(config: CoordinatorConfig) -> Self {
        let mut b = Self {
            config,
            regs: Vec::new(),
            descs: Vec::new(),
            kinds: Vec::new(),
            names: Vec::new(),
            edges: Vec::new(),
        };
        // The coordinator registers itself as system #0 under the reserved instance
        // name `"coordinator"` (`docs/design-command-slots.md` §2.6): an ordinary
        // declared bundle, so its channels are wired/sized/registered by the same
        // passes as every system's — no hand-rolled allocation blocks. Every output
        // is Host-connected (the coordinator itself holds the writers; a Host OUTPUT
        // still accepts consumer edges); the registry keys are byte-identical to the
        // historical hand-rolled ones (`coordinator.health` / `.log` /
        // `.coordinator_status` / `.sequences` / `.commands`).
        //
        // - `commands` is the operator channel behind the take-once
        //   [`Coordinator::control_handle`]; commands reach a slot only over an
        //   explicit `"coordinator" -> <slot>` edge. Untelemetered (inbound control
        //   is never echoed on the downlink).
        // - `sequences` carries the boot `SequenceRegistry` (telemetered — the
        //   panel's sequence view sources it).
        // - the `status` SelfTap is `read_status`'s view over the coordinator's own
        //   status output.
        let desc = SystemDescriptor {
            name: COORDINATOR_INSTANCE,
            kind: SystemKind::Cyclic,
            inputs: vec![
                PortDesc::of::<CoordinatorStatus>().with_conn(PortConn::SelfTap(
                    PortId::Component(CoordinatorStatus::FRAME_ID),
                )),
            ],
            outputs: vec![
                PortDesc::of::<crate::SystemHealth>().with_conn(PortConn::Host),
                PortDesc::of::<crate::SystemLog>().with_conn(PortConn::Host),
                PortDesc::of::<CoordinatorStatus>().with_conn(PortConn::Host),
                PortDesc::msg_named::<SequenceRegistry>("sequences")
                    .with_conn(PortConn::Host),
                PortDesc::msg_named::<SequenceCommand>("commands")
                    .untelemetered()
                    .with_conn(PortConn::Host),
            ],
            capabilities: Vec::new(),
        };
        b.descs.push(desc);
        b.kinds.push(SystemKind::Cyclic);
        b.names.push(COORDINATOR_INSTANCE.to_string());
        b.regs.push(Reg::Coordinator);
        b
    }

    /// The handle addressing the coordinator's own system-#0 bundle (§2.6), so a
    /// front-end can wire the operator command edge:
    /// `connect(PortRef::msg::<SequenceCommand>(b.coordinator_handle()), …)` — the
    /// Rust twin of the KDL `connect "coordinator" -> "<slot>" msg="SequenceCommand"`.
    pub fn coordinator_handle(&self) -> SystemHandle {
        SystemHandle { id: 0 }
    }

    /// The **registered** descriptor of `handle` — what `build()` validates, sizes,
    /// and wires. For a slot this is the derived contract (`add_slot` docs), which a
    /// front-end reads back instead of re-deriving; for everything else it is the
    /// system's own `descriptor()`.
    pub fn descriptor_of(&self, handle: SystemHandle) -> &SystemDescriptor {
        &self.descs[handle.id]
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
    /// observes every other system's fresh output. The downlink's `AllOutputs` receive-all
    /// port is what reserves it a reader slot on every buffer — `build()` derives that budget
    /// by counting `ReceiveAll` capabilities, so no manual bookkeeping is needed here.
    pub fn add_telemetry<T>(&mut self, config: TelemetryConfig<T>) -> SystemHandle
    where
        T: Transport + 'static,
    {
        self.add_cyclic_named("telemetry", TelemetrySystem::new(config))
    }

    /// Register the **uplink** (`docs/messages.md` §4.4): an ordinary [`AsyncSystem`] — the read
    /// twin of the telemetry downlink — that ingests panel `SequenceCommand` Msgs off `recv` and
    /// re-emits each onto its command channel (the §4.3 emit capability), which the head of
    /// [`run_for`](Coordinator::run_for) drains into the slots the **same cycle** it arrives.
    /// This is a thin convenience over [`add_async`](Self::add_async); the uplink is wired,
    /// sized, and spawned like any async system. A downlink-only mission omits it.
    ///
    /// The uplink owns its **own** connection (`recv`), distinct from the downlink's — a shared
    /// bidirectional link is deferred (`docs/messages.md` §4.5). The Mock test path supplies a
    /// [`RecvTransport`] directly.
    pub fn add_uplink<R>(&mut self, recv: R) -> SystemHandle
    where
        R: RecvTransport + 'static,
    {
        self.add_async(UplinkSystem::new(recv))
    }

    /// Register a dlopen'd cyclic system under an explicit instance name. `loaded` is
    /// an opened [`DlSystem`](crate::dl); `params` is the canonical postcard `Params`
    /// blob the `.so` decodes in `fsw_create` (identical on the wire from either
    /// front-end).
    ///
    /// This is the dl twin of [`add_cyclic_named`](Self::add_cyclic_named): it pushes
    /// the `.so`'s reconstructed [`SystemDescriptor`] so the **existing**
    /// `compatible()`/`WireError` validation and ring sizing/allocation run over it
    /// unchanged, and records a [`Reg::Dl`] registration whose `bind` (at `build()`)
    /// gathers the per-port ring regions, `fsw_create`s the state, and produces a
    /// [`DlSlot`](crate::dl) instead of a typed `CyclicRunner`. Its output buffers land
    /// in the [`Registry`] with the (prefixed) announce, so telemetry `All` taps
    /// them like a static system's.
    ///
    /// Dl systems are cyclic-only. This is the low-level builder method; the
    /// [`resolve`](crate::wiring::resolve) entry point drives it from a [`Wiring`](crate::Wiring)
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

    /// Register a runtime-swappable **slot** (sequences-slots.md §6): a fixed position in
    /// the cyclic call chain whose occupant the host `Load`s/`Start`s/`Stop`s/`Abort`s/
    /// `Reset`s/`Unload`s at runtime. `allowed` is the pre-opened candidate set — each
    /// `(Load-name, opened DlSystem, postcard params)` a sequence occupant. `initial`
    /// optionally applies one at startup.
    ///
    /// The registered descriptor is derived from the occupant descriptor **by
    /// extension, not surgery** (`docs/design-command-slots.md` §2.2): the occupant's
    /// ports form the *prefix* of each list in the occupant's own order (its trailing
    /// [`SlotControlIn`] input re-marked [`PortConn::Host`] in place — the runner
    /// holds the cancel writer), and the runner's ports are the *tail*: a declared
    /// `commands` `MsgIn<SequenceCommand>` fan-in (Edge — command wiring is ordinary
    /// message wiring, §2.5) and a [`PortConn::SelfTap`] view over the occupant's own
    /// [`SequenceStatus`] output on the input side; a [`SlotStatus`] output and the
    /// `"sequences"` events channel (both `Host`, registry-tapped) on the output
    /// side. [`SlotReg`] records the prefix split indices, so the bind arm maps the
    /// occupant `FswRing` arrays as a straight prefix walk — the occupant-side
    /// positional bind contract (and so the dl ABI) is untouched.
    ///
    /// Panics on a build-time contract violation: `allowed` is empty, an allowed
    /// occupant is not `compatible()` with the first occupant's contract, the
    /// occupant is not a v1 sequence shape (no trailing `SlotControlIn` input /
    /// `SequenceStatus` output), or `initial` names an occupant outside the allowed
    /// set (W1b — the KDL front-end surfaces the same checks as clean `LoadError`s
    /// before calling this).
    pub fn add_slot(
        &mut self,
        name: impl Into<String>,
        allowed: Vec<(String, crate::dl::DlSystem, Vec<u8>)>,
        initial: Option<InitialOccupant>,
    ) -> SystemHandle {
        assert!(
            !allowed.is_empty(),
            "a slot needs at least one allowed occupant"
        );
        let base = allowed[0].1.descriptor().clone();
        // Every allowed occupant must share the contract (the slot sizes/validates to the
        // first occupant's descriptor). v1 requires a shared shape (mutual subset).
        for (occ_name, sys, _) in &allowed[1..] {
            let d = sys.descriptor();
            let ports_match = |a: &[PortDesc], b: &[PortDesc]| {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|(x, y)| compatible(x, y) && compatible(y, x))
            };
            assert!(
                ports_match(&d.inputs, &base.inputs) && ports_match(&d.outputs, &base.outputs),
                "allowed occupant {occ_name:?} is incompatible with the slot contract \
                 (derived from {:?})",
                allowed[0].0
            );
        }
        // W1b: the builder path validates the initial occupant against the allowed
        // set at build, like the resolve path's `UnknownInitialOccupant` — a typo
        // would otherwise surface only as a runtime `Failed` event.
        if let Some(init) = &initial {
            assert!(
                allowed.iter().any(|(n, _, _)| n == &init.occupant),
                "initial occupant {:?} is not in the allowed set ({})",
                init.occupant,
                allowed
                    .iter()
                    .map(|(n, _, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        let name: String = name.into();
        // The registered descriptor name is the slot's instance name (a leaked
        // `&'static str` for the descriptor field + the `SlotRunner` identity).
        let leaked: &'static str = Box::leak(name.clone().into_boxed_str());

        // --- Inputs: occupant prefix (SlotControlIn re-marked Host), runner tail ---
        let mut inputs = base.inputs.clone();
        let control = inputs
            .iter_mut()
            .find(|p| p.id == PortId::Component(SlotControlIn::FRAME_ID))
            .expect("a v1 sequence occupant declares the implicit SlotControlIn input");
        // Host-connected: the RUNNER holds the cancel writer over a dedicated ring;
        // the occupant reads it. Exempt from UnconnectedInput; edges are rejected.
        control.conn = PortConn::Host;
        let n_occ_inputs = inputs.len();
        // The slot's declared command input (§2.5): an ordinary fan-in message
        // input, so command wiring is ordinary message wiring — a producer commands
        // this slot only over an explicit edge (`connect … msg="SequenceCommand"`).
        // Zero edges is legal (an autonomy-free, wiring-frozen slot). The runner
        // drains it at the head of each step; it is never handed to the occupant.
        inputs.push(PortDesc::msg::<SequenceCommand>());
        // The runner's read view over the occupant's own SequenceStatus output
        // (Progress lines + the terminal outcome): a declared self-tap — no ring,
        // +1 fan-out on that output at sizing time.
        let seq_status_id = PortId::Component(SequenceStatus::FRAME_ID);
        assert!(
            base.outputs.iter().any(|p| p.id == seq_status_id),
            "a v1 sequence occupant publishes a SequenceStatus output"
        );
        inputs.push(PortDesc::of::<SequenceStatus>().with_conn(PortConn::SelfTap(seq_status_id)));

        // --- Outputs: occupant prefix, runner tail (both Host, registry-tapped) ---
        let mut outputs = base.outputs.clone();
        let n_occ_outputs = outputs.len();
        // Host-side slot telemetry: the RUNNER writes the phase/occupant frame each
        // step; tapped like any output ("<slot>.slot_status").
        outputs.push(PortDesc::of::<SlotStatus>().with_conn(PortConn::Host));
        // The slot's events channel: the RUNNER emits a SequenceChannelEvent per
        // lifecycle transition; keyed "<slot>.sequences" (msg_named), telemetered.
        outputs.push(
            PortDesc::msg_named::<SequenceChannelEvent>("sequences").with_conn(PortConn::Host),
        );

        let registered = SystemDescriptor {
            name: leaked,
            kind: SystemKind::Cyclic,
            inputs,
            outputs,
            // Sequence occupants declare wired ports only (ReceiveAll is host-only).
            capabilities: Vec::new(),
        };

        let allowed = allowed
            .into_iter()
            .map(|(name, system, params)| AllowedOccupant {
                name,
                system,
                params,
            })
            .collect();

        let id = self.descs.len();
        self.descs.push(registered);
        self.kinds.push(SystemKind::Cyclic);
        self.names.push(name);
        self.regs.push(Reg::Slot(SlotReg {
            allowed,
            initial,
            n_occ_inputs,
            n_occ_outputs,
        }));
        SystemHandle { id }
    }

    /// Connect a producer output to a consumer input, addressed by port id. The full
    /// compatibility/structural validation runs in [`build`](Self::build); this only
    /// catches the cheap port-id and unknown-system/port mistakes early. **One entry
    /// point for every edge** (A7): the edge's behavior — fan-in rule, cycle-detection
    /// membership, lap policy — is inferred from the connected ports' descriptors, so
    /// a Snapshot (frame) edge and a Log (message) edge spell identically.
    ///
    /// A forward (acyclic) edge. If a `connect` happens to close a feedback loop over
    /// Snapshot edges — including a system connected to itself — `build` rejects it as
    /// a [`FeedbackCycle`](WireError::FeedbackCycle): the back-edge of a loop must be
    /// declared with [`connect_delayed`](Self::connect_delayed). Likewise a Snapshot
    /// edge between two cyclic systems must point *forward* in registration order (the
    /// step loop's execution order), or the consumer would permanently read last
    /// cycle's value — `build` rejects the backward edge as a
    /// [`StaleFrameEdge`](WireError::StaleFrameEdge) unless it is `connect_delayed`.
    /// Log edges are exempt from both (a decoupled event/command stream).
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
    /// Only meaningful on a Snapshot edge; `delayed` into a Log input is rejected at
    /// build as a [`DelayedLogEdge`](WireError::DelayedLogEdge).
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
        if producer.port != consumer.port {
            return Err(WireError::PortIdMismatch {
                producer: producer.port,
                consumer: consumer.port,
            });
        }
        self.edges.push((producer, consumer, delayed));
        Ok(())
    }

    /// Validate the graph, size and allocate every ring, bind ports, auto-provision
    /// health/log buffers, and return a ready coordinator (coordinator.md §2).
    pub fn build(mut self) -> Result<Coordinator, WireError> {
        // A Wall clock turns `cycle_rate` into the per-cycle pacing budget in `run_for`;
        // reject an unusable rate here so the failure is a build-time `WireError`, not a
        // `Duration::from_secs_f64` panic mid-run. A `Simulated` clock ignores the rate
        // (coordinator.md §6), so it is deliberately not validated there.
        if matches!(self.config.clock, ClockMode::Wall)
            && !(self.config.cycle_rate.is_finite() && self.config.cycle_rate > 0.0)
        {
            return Err(WireError::InvalidCycleRate {
                rate: self.config.cycle_rate,
            });
        }

        let n = self.descs.len();
        let depth = self.config.default_depth;

        // --- Receive-all (telemetry) systems register last (A11(b): enforced) --------
        // The downlink's end-of-cycle snapshot only observes systems stepping *before*
        // it, so a cyclic system registered after it would telemeter one cycle stale.
        // Enforced, not silently reordered: reordering registrations would change the
        // step order the stale-edge diagnostics validate. Async systems are exempt
        // (they run off their own task, not the registration-ordered step loop).
        let mut first_receive_all: Option<usize> = None;
        for s in 0..n {
            if self.kinds[s] != SystemKind::Cyclic {
                continue;
            }
            let has_receive_all = self.descs[s]
                .capabilities
                .contains(&crate::Capability::ReceiveAll);
            if has_receive_all {
                first_receive_all.get_or_insert(s);
            } else if let Some(t) = first_receive_all {
                return Err(WireError::ReceiveAllNotLast {
                    system: self.names[s].clone(),
                    receive_all: self.names[t].clone(),
                });
            }
        }

        // --- Slot instance names are wire addresses: enforce the NAME_CAP ------------
        // A `SequenceCommand` addresses a slot by its instance name, and the same name
        // packs into fixed-size status frames; > NAME_CAP would telemeter truncated
        // while addressing untruncated, so it is a build error, never a truncation.
        for s in 0..n {
            if matches!(self.regs[s], Reg::Slot(_)) && self.descs[s].name.len() > NAME_CAP {
                return Err(WireError::SlotNameTooLong {
                    name: self.descs[s].name.to_string(),
                    len: self.descs[s].name.len(),
                });
            }
        }

        // --- Per-descriptor axis validation (no edges needed) ----------------
        // FanIn::Many × Delivery::Snapshot: latest-wins across producers is
        // ill-defined. OnLap::Stop on an async system's input: an async consumer
        // cannot be step-gated, so the hard-stop doctrine is unenforceable there
        // (§2.3). A Snapshot input *defaults* Stop (the cyclic doctrine), so on an
        // async system that combination is the documented coercion — effectively
        // Resync, implemented by the drop-on-full copy-in ring — not an error; only
        // the non-default Log × Stop (necessarily an explicit declaration, since Log
        // defaults Resync) is rejected, because nothing decouples a Log input.
        for s in 0..n {
            for port in &self.descs[s].inputs {
                if port.fan_in == FanIn::Many && port.delivery == Delivery::Snapshot {
                    return Err(WireError::SnapshotFanIn {
                        system: self.descs[s].name,
                        port: port.id,
                    });
                }
                if self.kinds[s] == SystemKind::Async
                    && port.on_lap == OnLap::Stop
                    && port.delivery == Delivery::Log
                {
                    return Err(WireError::StopOnAsyncInput {
                        system: self.descs[s].name,
                        port: port.id,
                    });
                }
            }
        }

        // --- Validate edges, build the ONE connection map ---------------------
        // (cons_id, in_idx) -> [(prod_id, out_idx)] for EVERY input — a FanIn::One
        // input holds exactly one entry (enforced below), a FanIn::Many input zero or
        // more. Every rule from here on branches on a descriptor axis, never on
        // frame-vs-message.
        let mut cons_edges: HashMap<(usize, usize), Vec<(usize, usize)>> = HashMap::new();
        // System-level adjacency over the NON-delayed SNAPSHOT edges only, for cycle
        // detection: a remaining cycle is an unbroken feedback loop. Log edges are
        // excluded — a log is a decoupled event/command stream, not a same-cycle
        // dependency (§3.6).
        let mut forward_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (p, c, delayed) in &self.edges {
            let out_idx = self.descs[p.system.id]
                .outputs
                .iter()
                .position(|d| d.id == p.port)
                .ok_or(WireError::UnknownPort {
                    system: p.system.id,
                    port: p.port,
                })?;
            let in_idx = self.descs[c.system.id]
                .inputs
                .iter()
                .position(|d| d.id == c.port)
                .ok_or(WireError::UnknownPort {
                    system: c.system.id,
                    port: c.port,
                })?;
            if !compatible(
                &self.descs[p.system.id].outputs[out_idx],
                &self.descs[c.system.id].inputs[in_idx],
            ) {
                return Err(WireError::Incompatible {
                    producer: self.descs[p.system.id].name,
                    consumer: self.descs[c.system.id].name,
                    port: c.port,
                });
            }
            let in_desc = &self.descs[c.system.id].inputs[in_idx];
            // A host-connected input's counterpart is held by the system's runner
            // (the slot's cancel writer, a self-tap over its own output) — an edge
            // into it is rejected (A3). Host *outputs* keep accepting consumer edges:
            // the coordinator's `commands` channel is exactly a Host output slots
            // read over explicit edges.
            if in_desc.conn != PortConn::Edge {
                return Err(WireError::HostPort {
                    system: self.descs[c.system.id].name,
                    port: c.port,
                });
            }
            // `delayed` marks a one-cycle-late snapshot sample; on a Log input it is
            // meaningless and now rejected instead of silently ignored (A7).
            if *delayed && in_desc.delivery == Delivery::Log {
                return Err(WireError::DelayedLogEdge {
                    producer: self.descs[p.system.id].name,
                    consumer: self.descs[c.system.id].name,
                    port: c.port,
                });
            }
            let producers = cons_edges.entry((c.system.id, in_idx)).or_default();
            match in_desc.fan_in {
                // Exactly one edge per input (the frame doctrine).
                FanIn::One => {
                    if !producers.is_empty() {
                        return Err(WireError::DoubleConnect {
                            system: self.descs[c.system.id].name,
                            port: c.port,
                        });
                    }
                    producers.push((p.system.id, out_idx));
                }
                // Fan-in (append). Distinct producers may fan in freely (§3.2), but an
                // exact duplicate of one edge would deliver every record twice (B7).
                FanIn::Many => {
                    if producers.contains(&(p.system.id, out_idx)) {
                        return Err(WireError::DuplicateEdge {
                            producer: self.descs[p.system.id].name,
                            consumer: self.descs[c.system.id].name,
                            port: c.port,
                        });
                    }
                    producers.push((p.system.id, out_idx));
                }
            }
            // Self-edges included: a system plainly connected to itself is the
            // tightest feedback loop (it can only ever read its own previous
            // cycle's value), so it must be declared with `connect_delayed` like
            // any other loop — the DFS reports it as a one-member `FeedbackCycle`.
            if in_desc.delivery == Delivery::Snapshot && !delayed {
                forward_adj[p.system.id].push(c.system.id);
            }
        }

        // --- Every feedback loop must be broken by a `connect_delayed` --------
        if let Some(cycle) = find_cycle(&forward_adj) {
            return Err(WireError::FeedbackCycle {
                systems: cycle.into_iter().map(|id| self.descs[id].name).collect(),
            });
        }

        // --- Registration order must agree with the dataflow ------------------
        // The cyclic step loop runs in registration order, so a non-delayed Snapshot
        // edge between two cyclic systems whose consumer registered *before* its
        // producer would read last cycle's value forever — silent staleness that must
        // instead be declared with `connect_delayed`. Checked after cycle detection so
        // a genuine unbroken loop (which always contains a backward edge) reports the
        // clearer `FeedbackCycle`. Log edges are exempt (a decoupled stream, §3.6); so
        // are edges with an async endpoint (async systems run off the post-step
        // copy-in / their own task, so their registration index carries no ordering
        // semantics). Self-edges never reach here (rejected above as a one-member cycle).
        for (p, c, delayed) in &self.edges {
            if *delayed {
                continue;
            }
            let in_delivery = self.descs[c.system.id]
                .inputs
                .iter()
                .find(|d| d.id == c.port)
                .map(|d| d.delivery);
            if in_delivery != Some(Delivery::Snapshot) {
                continue;
            }
            let both_cyclic = self.kinds[p.system.id] == SystemKind::Cyclic
                && self.kinds[c.system.id] == SystemKind::Cyclic;
            if both_cyclic && c.system.id < p.system.id {
                return Err(WireError::StaleFrameEdge {
                    producer: self.descs[p.system.id].name,
                    consumer: self.descs[c.system.id].name,
                    port: c.port,
                });
            }
        }

        // --- Input coverage: a FanIn::One input must be connected exactly once ---
        // (exactly-once is the edge pass above; existence is here). A FanIn::Many
        // input may have zero producers (fan-in is optional, §3.2); a non-Edge input
        // is fed by its runner, never an edge (A3).
        for s in 0..n {
            for (in_idx, port) in self.descs[s].inputs.iter().enumerate() {
                if port.conn == PortConn::Edge
                    && port.fan_in == FanIn::One
                    && !cons_edges.contains_key(&(s, in_idx))
                {
                    return Err(WireError::UnconnectedInput {
                        system: self.descs[s].name,
                        port: port.id,
                    });
                }
            }
        }

        // --- Fan-out per output port: one uniform count over the one map -----
        let mut fan_out: HashMap<(usize, usize), usize> = HashMap::new();
        for producers in cons_edges.values() {
            for &(prod_id, out_idx) in producers {
                *fan_out.entry((prod_id, out_idx)).or_insert(0) += 1;
            }
        }
        // A declared self-tap (A3) is one more reader on the system's *own* output —
        // counted here so the budget is explicit, not slack-covered.
        for s in 0..n {
            for port in &self.descs[s].inputs {
                let PortConn::SelfTap(pid) = port.conn else {
                    continue;
                };
                let out_idx = self.descs[s]
                    .outputs
                    .iter()
                    .position(|o| o.id == pid)
                    .expect("a SelfTap names one of the system's own outputs");
                *fan_out.entry((s, out_idx)).or_insert(0) += 1;
            }
        }

        let mut table = RingTable { rings: Vec::new() };
        // The build-order registry entries — ONE list over every registered buffer,
        // frames and message channels alike (telemetry.md §2). Collected alongside
        // allocation and frozen into an `Arc<Registry>` *before* the bind loop, so a
        // system can pull it in `BindPorts::bind` (telemetry.md §2.3).
        let mut reg_entries: Vec<RegistryEntry> = Vec::new();
        // Each `AllOutputs` receive-all capability (`docs/message-wiring.md` §4) is an extra
        // fan-out reader on *every* output + message buffer, so `build()` sizes every ring's
        // `max_readers` to include it. Self-derived from the declared `ReceiveAll`
        // capabilities across all systems — no manual `add_telemetry` bookkeeping.
        let n_reg = self
            .descs
            .iter()
            .flat_map(|d| d.capabilities.iter())
            .filter(|c| **c == crate::Capability::ReceiveAll)
            .count();

        // --- Allocate one buffer per output port (incl. health/log) ----------
        let mut output_rings: Vec<Vec<RingBuffer<BoxBacking>>> = Vec::with_capacity(n);
        for s in 0..n {
            let mut row = Vec::with_capacity(self.descs[s].outputs.len());
            for (out_idx, port) in self.descs[s].outputs.iter().enumerate() {
                let readers =
                    fan_out.get(&(s, out_idx)).copied().unwrap_or(0) + n_reg + READER_SLACK;
                let instance = self.names[s].clone();
                let role = BufferRole::Output {
                    system: s,
                    port: out_idx,
                };
                // ONE sizing path (depth by delivery — `alloc_ring`); only the
                // registry-entry shape still splits on the schema. Command channels
                // are ordinary outputs here: a slot reads a producer only over an
                // explicit edge, so the edge fan-out counts its readers exactly (A2).
                let ring = alloc_ring(port.delivery, port.max_size, depth, readers);
                match &port.schema {
                    PortSchema::Table { .. } => {
                        reg_entries.push(registry_entry(&instance, port, ring.clone()));
                        table.rings.push(RingEntry {
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
                        // Registered like any buffer; the downlink / `AllOutputs`
                        // taps it unless the port opted out via
                        // `telemetered = false` (e.g. a command channel, §6.4).
                        let entry =
                            postcard_entry(&instance, port.name, ring.clone(), port.telemetered);
                        table.rings.push(RingEntry {
                            ring: ring.clone(),
                            frame_id: entry.key,
                            role,
                            instance: Some(instance),
                        });
                        reg_entries.push(entry);
                    }
                }
                row.push(ring);
            }
            output_rings.push(row);
        }

        // --- Dedicated rings for Host-connected inputs (A3) -------------------
        // A Host input's counterpart is its runner's writer (the slot's cancel
        // frame), so it gets its own ring instead of a producer edge: the occupant
        // attaches one read `View` per Load (released on each Stop/Reset/Unload), so
        // 1 reader slot + slack covers the reload cycle. No registry entry — it is
        // inbound control, not an output. SelfTap inputs allocate nothing (they view
        // the system's own output, already counted in `fan_out`).
        let mut host_input_rings: HashMap<(usize, usize), RingBuffer<BoxBacking>> =
            HashMap::new();
        for s in 0..n {
            for (in_idx, port) in self.descs[s].inputs.iter().enumerate() {
                if port.conn != PortConn::Host {
                    continue;
                }
                let ring = alloc_ring(port.delivery, port.max_size, depth, 1 + READER_SLACK);
                table.rings.push(RingEntry {
                    ring: ring.clone(),
                    frame_id: port
                        .id
                        .component()
                        .expect("v1 host-connected inputs are table ports"),
                    role: BufferRole::Private {
                        system: s,
                        input: in_idx,
                    },
                    instance: Some(self.names[s].clone()),
                });
                host_input_rings.insert((s, in_idx), ring);
            }
        }

        // --- Boot `SequenceRegistry` payload -----------------------------------
        // One spec per slot, keyed by the slot's **instance name** — the channel's
        // wire address (`docs/messages.md` §5); there is no build-order channel id.
        let mut seq_specs: Vec<SequenceChannelSpec> = Vec::new();
        for (s, reg) in self.regs.iter().enumerate() {
            if let Reg::Slot(slot_reg) = reg {
                seq_specs.push(SequenceChannelSpec {
                    name: self.descs[s].name.to_string(),
                    available: slot_reg.allowed.iter().map(|a| a.name.clone()).collect(),
                });
            }
        }

        let seq_registry = SequenceRegistry {
            channels: seq_specs,
        };

        // One keyspace: a same-instance name collision between a frame and a channel
        // (both `"<instance>.<name>"`) is now detectable instead of shadowing.
        let mut seen_keys: HashMap<ComponentId, usize> = HashMap::new();
        for (i, e) in reg_entries.iter().enumerate() {
            if seen_keys.insert(e.key, i).is_some() {
                return Err(WireError::DuplicateRegistryKey {
                    key: format!("{}.{}", e.instance, e.name),
                });
            }
        }

        // Freeze the ONE registry; every consumer's bind pulls this same handle.
        let registry = Arc::new(Registry::new(reg_entries));

        // --- Private copy-in buffers for async inputs ------------------------
        // Keyed on the delivery axis (§2.3): an async system cannot be step-gated, so
        // an async SNAPSHOT input is effectively Resync, implemented by a private
        // drop-on-full copy-in ring (which also supplies the matched data `Notifier`
        // the async `recv` parks on). Log inputs use a direct fan-in multi-view
        // (§3.3) — a best-effort log the consumer poll-drains, no copy-in.
        // (async_sys, in_idx) -> (private ring, matched data notifier)
        let mut private_inputs: HashMap<(usize, usize), (RingBuffer<BoxBacking>, Notifier)> =
            HashMap::new();
        let mut async_wakes: Vec<Vec<Notifier>> = vec![Vec::new(); n];
        let mut copy_ins: Vec<CopyIn> = Vec::new();
        for s in 0..n {
            if self.kinds[s] != SystemKind::Async {
                continue;
            }
            for in_idx in 0..self.descs[s].inputs.len() {
                // Only edge-connected Snapshot inputs are copy-in decoupled; a
                // Host/SelfTap input is fed by its runner, not a producer edge (A3).
                if self.descs[s].inputs[in_idx].delivery == Delivery::Log
                    || self.descs[s].inputs[in_idx].conn != PortConn::Edge
                {
                    continue;
                }
                let (prod_id, out_idx) = cons_edges[&(s, in_idx)][0];
                let port = &self.descs[s].inputs[in_idx];
                let private = alloc_ring(port.delivery, port.max_size, depth, 1 + READER_SLACK);
                let data = Notifier::default();
                // The private ring is Overwrite, so a write never suspends for space —
                // only the matched DATA notifier is load-bearing (it wakes the parked
                // async `recv`); the writer's space side is `NoWake`.
                // Invariant: each private copy-in ring is created here and gets
                // this one writer, so the claim is always free.
                let writer = private
                    .writer(data.clone(), NoWake)
                    .expect("private copy-in ring has exactly one writer");
                let upstream = output_rings[prod_id][out_idx]
                    .view(NoWake, NoWake)
                    .expect("producer reader slot reserved at sizing time");
                copy_ins.push(CopyIn {
                    upstream,
                    writer,
                    scratch: Vec::new(),
                });
                private_inputs.insert((s, in_idx), (private.clone(), data.clone()));
                async_wakes[s].push(data);
                table.rings.push(RingEntry {
                    ring: private,
                    frame_id: port.id.component().expect("copy-in inputs are table ports"),
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
        // The coordinator's own (#0) ports, wrapped by its bind arm below and
        // unwrapped after the loop (`Reg::Coordinator` is always registered first).
        let mut control_out: Option<MsgOut<SequenceCommand>> = None;
        let mut coord_health: Option<HealthPort> = None;
        let mut status_out: Option<Output<CoordinatorStatus>> = None;
        let mut status_view: Option<Input<CoordinatorStatus>> = None;
        let mut seq_registry_out: Option<MsgOut<SequenceRegistry>> = None;
        let regs = std::mem::take(&mut self.regs);
        for (id, reg) in regs.into_iter().enumerate() {
            match reg {
                // The coordinator's own bundle (§2.6): a marker registration — not a
                // cyclic slot (the coordinator IS the loop). Its declared Host outputs
                // were allocated/registered by the uniform passes above; wrap the
                // writers into the coordinator's fields here, single-writer by
                // construction, and claim the status SelfTap view (`read_status`).
                Reg::Coordinator => {
                    let desc = &self.descs[id];
                    let out_idx = |pid: PortId| {
                        desc.outputs
                            .iter()
                            .position(|p| p.id == pid)
                            .expect("the coordinator #0 bundle declares this output")
                    };
                    let health_ring =
                        &output_rings[id][out_idx(PortId::Component(SystemHealth::FRAME_ID))];
                    let log_ring =
                        &output_rings[id][out_idx(PortId::Component(SystemLog::FRAME_ID))];
                    coord_health = Some(HealthPort::new(
                        slot_writer::<SystemHealth>(health_ring),
                        slot_writer::<SystemLog>(log_ring),
                    ));
                    let status_idx = out_idx(PortId::Component(CoordinatorStatus::FRAME_ID));
                    status_out =
                        Some(slot_writer::<CoordinatorStatus>(&output_rings[id][status_idx]));
                    // The declared SelfTap over the coordinator's own status output
                    // (+1 fan-out counted at sizing).
                    status_view = Some(Input::new(
                        output_rings[id][status_idx]
                            .view(NoWake, NoWake)
                            .expect("status self-tap reader (fan-out sized)"),
                    ));
                    seq_registry_out = Some(owned_writer::<SequenceRegistry>(
                        &output_rings[id][out_idx(PortId::Packet(SequenceRegistry::ID))],
                    ));
                    control_out = Some(owned_writer::<SequenceCommand>(
                        &output_rings[id][out_idx(PortId::Packet(SequenceCommand::ID))],
                    ));
                }
                // A dlopen'd system binds over **raw** `FswRing` regions, not typed
                // `BoundPort`s: gather the same per-port rings the coordinator allocated
                // (outputs = this system's own buffers; inputs = views into the upstream
                // producers' outputs — the cyclic-consumer path), as `(base, len, role)`
                // handles in `descriptors()` order, and hand them to a `DlSlot`.
                // Sizing, allocation, validation, and the registry entry above are
                // identical to a static system's.
                Reg::Dl(dl) => {
                    use crate::abi::{FswRing, ROLE_INPUT, ROLE_OUTPUT};
                    let outputs: Vec<FswRing> = (0..self.descs[id].outputs.len())
                        .map(|out_idx| {
                            let (base, len) = output_rings[id][out_idx].region();
                            FswRing {
                                base,
                                len,
                                role: ROLE_OUTPUT,
                            }
                        })
                        .collect();
                    let inputs: Vec<FswRing> = (0..self.descs[id].inputs.len())
                        .map(|in_idx| {
                            let (prod_id, out_idx) = cons_edges[&(id, in_idx)][0];
                            let (base, len) = output_rings[prod_id][out_idx].region();
                            FswRing {
                                base,
                                len,
                                role: ROLE_INPUT,
                            }
                        })
                        .collect();
                    // SAFETY: every region named here is a `RingTable`-owned ring that
                    // outlives the slot — the coordinator drops `cyclic` (this slot,
                    // whose `Drop` calls `fsw_destroy`) before `rings`.
                    let slot = unsafe {
                        dl.system
                            .into_slot(&dl.params, inputs, outputs, self.descs[id].name)
                    };
                    cyclic.push(Box::new(slot));
                }
                // A runtime slot: gather the same per-port regions as the Dl arm, but
                // append the slot's owned control ring to the occupant's input array and
                // hand the runner the control/status writers. No occupant is created here
                // — only `init`/`Load` (runtime) does.
                Reg::Slot(slot_reg) => {
                    use crate::abi::{FswRing, ROLE_INPUT, ROLE_OUTPUT};
                    let SlotReg {
                        allowed,
                        initial,
                        n_occ_inputs,
                        n_occ_outputs,
                    } = slot_reg;
                    let desc = &self.descs[id];
                    // The prefix/tail invariant (§2.2): the occupant's ports are the
                    // prefix of each registered list, in the occupant descriptor's own
                    // order — so the occupant `FswRing` arrays are a straight prefix
                    // map (Edge inputs view their producers; the Host `SlotControlIn`
                    // input its dedicated ring) and the occupant-side positional bind
                    // contract (the dl ABI) is untouched.
                    let inputs: Vec<FswRing> = (0..n_occ_inputs)
                        .map(|in_idx| {
                            let (base, len) = match desc.inputs[in_idx].conn {
                                PortConn::Edge => {
                                    let (prod_id, out_idx) = cons_edges[&(id, in_idx)][0];
                                    output_rings[prod_id][out_idx].region()
                                }
                                PortConn::Host => host_input_rings[&(id, in_idx)].region(),
                                PortConn::SelfTap(_) => {
                                    unreachable!("the occupant input prefix holds no self-tap")
                                }
                            };
                            FswRing {
                                base,
                                len,
                                role: ROLE_INPUT,
                            }
                        })
                        .collect();
                    // Occupant outputs = the prefix of the slot's own buffers (user
                    // outputs + SequenceStatus + health + log, in descriptor order).
                    let outputs: Vec<FswRing> = (0..n_occ_outputs)
                        .map(|out_idx| {
                            let (base, len) = output_rings[id][out_idx].region();
                            FswRing {
                                base,
                                len,
                                role: ROLE_OUTPUT,
                            }
                        })
                        .collect();

                    // --- The runner's tail ports, located by their declared shape ---
                    // Host cancel writer over the SlotControlIn input's dedicated ring.
                    let control_in_idx = desc.inputs[..n_occ_inputs]
                        .iter()
                        .position(|p| p.conn == PortConn::Host)
                        .expect("a slot declares its Host SlotControlIn input");
                    let control = slot_writer::<SlotControlIn>(
                        &host_input_rings[&(id, control_in_idx)],
                    );
                    // The slot's command fan-in: one view per producer explicitly
                    // edged into the declared `commands` input (A2 — no type-keyed
                    // broadcast; zero edges is a legal, command-less slot). The
                    // `SlotRunner` drains + filters by its instance name each step.
                    let cmd_in_idx = desc
                        .inputs
                        .iter()
                        .position(|p| {
                            p.conn == PortConn::Edge
                                && p.id == PortId::Packet(SequenceCommand::ID)
                        })
                        .expect("a slot's registered descriptor declares its commands input");
                    let commands = MsgIn::from_views(
                        cons_edges
                            .get(&(id, cmd_in_idx))
                            .map(|producers| {
                                producers
                                    .iter()
                                    .map(|&(prod_id, out_idx)| {
                                        output_rings[prod_id][out_idx]
                                            .view(NoWake, NoWake)
                                            .expect("command reader slot (edge fan-out sized)")
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    );
                    // The declared self-tap over the occupant's own SequenceStatus
                    // output (+1 fan-out counted at sizing) — Progress + outcome.
                    let seq_tap = desc
                        .inputs
                        .iter()
                        .find_map(|p| match p.conn {
                            PortConn::SelfTap(pid) => Some(pid),
                            _ => None,
                        })
                        .expect("a slot declares its SequenceStatus self-tap");
                    let seq_out_idx = desc
                        .outputs
                        .iter()
                        .position(|o| o.id == seq_tap)
                        .expect("a SelfTap names one of the slot's own outputs");
                    let seq_status = Input::new(
                        output_rings[id][seq_out_idx]
                            .view(NoWake, NoWake)
                            .expect("SequenceStatus self-tap reader (fan-out sized)"),
                    );
                    // Host writers over the runner's declared output tail: SlotStatus
                    // + the "sequences" events channel (real output indices now — no
                    // off-by-the-end BufferRole, no side allocation).
                    let status_out_idx = desc
                        .outputs
                        .iter()
                        .position(|o| o.id == PortId::Component(SlotStatus::FRAME_ID))
                        .expect("a slot declares its Host SlotStatus output");
                    let status_out =
                        slot_writer::<SlotStatus>(&output_rings[id][status_out_idx]);
                    let events_out_idx = desc
                        .outputs
                        .iter()
                        .position(|o| o.id == PortId::Packet(SequenceChannelEvent::ID))
                        .expect("a slot declares its Host events output");
                    let events = owned_writer::<SequenceChannelEvent>(
                        &output_rings[id][events_out_idx],
                    );

                    let runner = SlotRunner::new(
                        self.descs[id].name,
                        allowed,
                        initial,
                        inputs,
                        outputs,
                        control,
                        status_out,
                        events,
                        seq_status,
                        commands,
                    );
                    cyclic.push(Box::new(runner));
                }
                // The static (host-side `BoxBacking`) path: build typed `BoundPort`s and
                // walk them with a `Binder`.
                reg => {
                    // Outputs: default wakes, the system's own buffers. Capabilities
                    // never appear here — they live on `descs[id].capabilities`, not
                    // in the port lists, so the positional cursor covers exactly the
                    // wired ports (`AllOutputs::bind` pulls the registry instead of
                    // consuming a cursor position).
                    let outs: Vec<BoundPort> = (0..self.descs[id].outputs.len())
                        .map(|out_idx| BoundPort::new(output_rings[id][out_idx].clone()))
                        .collect();
                    // Inputs, in `descriptors()` order, chosen by the FAN-IN axis. A
                    // `One` input: cyclic consumers view the producer's output
                    // directly, async consumers view their private copy-in buffer with
                    // the matched data wake. A `Many` input: a direct multi-view over
                    // every producer ring wired to it (fan-in, §3.3), NoWake — a
                    // best-effort log the consumer poll-drains (no copy-in, cyclic or
                    // async).
                    let ins: Vec<BoundInput> = (0..self.descs[id].inputs.len())
                        .map(|in_idx| match self.descs[id].inputs[in_idx].fan_in {
                            FanIn::One => {
                                let (prod_id, out_idx) = cons_edges[&(id, in_idx)][0];
                                // The copy-in pass keyed on (Async, Snapshot); anything
                                // it decoupled binds the private ring + matched wake,
                                // everything else views the producer directly.
                                let port = match private_inputs.get(&(id, in_idx)) {
                                    Some((ring, data)) => {
                                        BoundPort::matched(ring.clone(), Box::new(data.clone()))
                                    }
                                    None => {
                                        BoundPort::new(output_rings[prod_id][out_idx].clone())
                                    }
                                };
                                BoundInput::One(port)
                            }
                            FanIn::Many => {
                                let ports = cons_edges
                                    .get(&(id, in_idx))
                                    .map(|producers| {
                                        producers
                                            .iter()
                                            .map(|&(prod_id, out_idx)| {
                                                BoundPort::new(output_rings[prod_id][out_idx].clone())
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                BoundInput::Many(ports)
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
                        // The dl/slot/coordinator arms are handled by the outer match.
                        Reg::Dl(_) => unreachable!("dl registration bound by the outer match"),
                        Reg::Slot(_) => unreachable!("slot registration bound by the outer match"),
                        Reg::Coordinator => {
                            unreachable!("coordinator registration bound by the outer match")
                        }
                    }
                }
            }
        }

        // The coordinator's own ports were wrapped by its #0 bind arm — always
        // registered (CoordinatorBuilder::new), so the unwraps are structural.
        let coord_health = coord_health.expect("coordinator #0 bound its health port");
        let status_out = status_out.expect("coordinator #0 bound its status writer");
        let status_view = status_view.expect("coordinator #0 bound its status view");
        let seq_registry_out =
            seq_registry_out.expect("coordinator #0 bound its sequences writer");

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
            progress: Arc::new(AtomicU64::new(0)),
            registry,
            control_out,
            seq_registry_out,
            seq_registry,
            seq_registry_emitted: false,
            started: false,
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
        if color[s] == WHITE {
            if let Some(c) = dfs(s, adj, &mut color, &mut stack) {
                return Some(c);
            }
        }
    }
    None
}

/// The `Simulated` per-cycle timestamp: `epoch + k*dt`, computed in wide integer
/// nanoseconds so the cycle index is never truncated (the previous `dt * k as u32`
/// wrapped the timeline back to `epoch` every 2³² cycles, breaking monotonicity and
/// stalling in-flight `Wait`s). The u128 product cannot overflow for any realistic
/// run; the final i64-microsecond cast holds ~292k years of simulated time.
fn simulated_now(epoch: Timestamp, dt: Duration, k: u64) -> Timestamp {
    Timestamp(epoch.0 + (dt.as_nanos() * k as u128 / 1_000) as i64)
}

/// Whether the freshly scanned stopped set differs from the previously published one.
/// Both slices come from the same in-order scan of the cyclic slots, so an element-wise
/// `(name, reason)` compare is an exact membership compare — no set structure, no
/// allocation. A length-only check is not enough: stops are no longer monotonic (a slot
/// recovers via `Load`/`Reset`), so slot A can recover the same cycle slot B stops,
/// changing the membership while the count stays put.
fn stopped_set_changed(cur: &[StoppedSystem], prev: &[StoppedSystem]) -> bool {
    cur.len() != prev.len()
        || cur
            .iter()
            .zip(prev)
            .any(|(a, b)| a.name != b.name || a.reason != b.reason)
}

/// The ONE ring-sizing helper (`docs/design-port-unification.md` §4 PASS 5): a
/// Snapshot port is sized at the configured default depth (a latest-wins sample needs
/// little history), a Log port at [`LOG_DEPTH`] (an every-record stream must absorb a
/// slow tap). Overwrite, like every owned ring.
fn alloc_ring(
    delivery: Delivery,
    max_size: usize,
    default_depth: usize,
    max_readers: usize,
) -> RingBuffer<BoxBacking> {
    let depth = match delivery {
        Delivery::Snapshot => default_depth,
        Delivery::Log => LOG_DEPTH,
    };
    RingBuffer::create_in_memory(Config {
        capacity: capacity_for(max_size, depth),
        max_readers,
        overrun: Overrun::Overwrite,
    })
}

/// Mint the single [`MsgOut`] writer over a coordinator-owned ring — the
/// [`slot_writer`] analogue for the message channel, exactly how the coordinator
/// mints its own `status_out`/`control` writers. Called exactly once per ring at
/// build (the region's writer claim enforces it).
fn owned_writer<M: Msg>(ring: &RingBuffer<BoxBacking>) -> MsgOut<M> {
    // Invariant: each coordinator-owned message ring gets its single writer
    // minted exactly once at build, so the claim is always free here.
    let writer = ring
        .writer(NoWake, NoWake)
        .expect("coordinator message ring has exactly one writer");
    MsgOut::new(writer)
}

/// Build a Postcard [`RegistryEntry`] for one message channel: the instance-qualified
/// key `ComponentId::new("<instance>.<name>")` (the on-wire identity) over a clone of
/// the ring — the [`registry_entry`] sibling for the self-describing record
/// (`docs/messages.md` §2). No vtable/announce; the record's 2-byte id is the schema.
fn postcard_entry(
    instance: &str,
    name: &str,
    ring: RingBuffer<BoxBacking>,
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

/// The synthetic instance prefix coordinator-owned buffers downlink under
/// (telemetry.md §6): they have no system instance, so their qualified key is
/// `coordinator.health` / `coordinator.log` / `coordinator.coordinator_status`.
const COORDINATOR_INSTANCE: &str = "coordinator";

/// Build a [`RegistryEntry`] for one buffer: compute the instance-qualified key and
/// the prefixed announce vtable+metadata once (telemetry.md §2.1/§6), capturing a
/// clone of the ring as the read source.
fn registry_entry(instance: &str, port: &PortDesc, ring: RingBuffer<BoxBacking>) -> RegistryEntry {
    let key = ComponentId::new(&format!("{instance}.{}", port.name));
    // Invariant: only Table ports come through here (the caller branches on the
    // schema), so the checked accessors are always `Some`.
    // `announce` is an `Arc<dyn Fn>` (not directly callable); deref to a `&dyn Fn`.
    let announce = port.announce().expect("table port carries an announce factory");
    let (vtable, metadata) = (**announce)(instance);
    RegistryEntry {
        key,
        instance: Arc::from(instance),
        name: Arc::from(port.name),
        schema: EntrySchema::Table {
            frame_id: port.id.component().expect("table port keys on a ComponentId"),
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
    /// A shared, lock-free mirror of `cycle` that an out-of-loop observer (e.g. the CLI
    /// runner's heartbeat) can read while `run_for` holds `&mut self` — published each
    /// cycle. Purely observational; nothing in the loop reads it back.
    progress: Arc<AtomicU64>,
    /// The ONE broad registry over every registered buffer — frames and message
    /// channels alike, untelemetered entries included (telemetry.md §2).
    registry: Arc<Registry>,
    /// The single writer over the coordinator's declared `commands` output (its #0
    /// bundle, §2.6): the in-proc `SequenceCommand` producer. A slot reads it only
    /// over an explicit `"coordinator" -> <slot>` edge — the same wiring surface the
    /// uplink uses; with no edge the handle is inert (visible in the graph, A2).
    /// Minted once at `build()` (the ring's writer claim enforces one live writer)
    /// and handed out by [`control_handle`](Coordinator::control_handle), which
    /// takes it — `None` afterwards.
    control_out: Option<MsgOut<SequenceCommand>>,
    /// The sole writer of the coordinator's boot-`SequenceRegistry` message channel (§5).
    seq_registry_out: MsgOut<SequenceRegistry>,
    /// The prebuilt boot [`SequenceRegistry`] payload (the slots + their allowed
    /// occupants), emitted once at the head of [`run_for`](Coordinator::run_for).
    seq_registry: SequenceRegistry,
    /// Whether the boot `SequenceRegistry` has been emitted (emit-once; the re-emit hook
    /// for a future `ReloadSequences` is [`emit_sequence_registry`](Coordinator::emit_sequence_registry)).
    seq_registry_emitted: bool,
    /// Latched by the first [`run_for`](Coordinator::run_for): a run consumes the
    /// coordinator (spawned async systems and their transports are gone after
    /// shutdown), so a second run would silently re-init everything over dead
    /// plumbing — it panics instead.
    started: bool,
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

    /// A shared handle to the live cycle counter (0 before the first cycle), readable
    /// while [`run_for`](Self::run_for) is running — e.g. for a progress heartbeat on
    /// another task. Lock-free, observational only.
    pub fn progress(&self) -> Arc<AtomicU64> {
        self.progress.clone()
    }

    /// The ONE broad registry over every registered buffer (telemetry.md §2): an
    /// index a logger, recorder, debugger, or test can use to read any buffer —
    /// frame output or message channel — by its instance-qualified id
    /// `ComponentId::new("<instance>.<name>")`. Unfiltered: untelemetered entries
    /// (e.g. `coordinator.commands`) are visible here, unlike through
    /// [`AllOutputs`](crate::AllOutputs).
    pub fn registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }

    /// Emit the boot [`SequenceRegistry`] on the coordinator's message channel (§5): the
    /// slots and their allowed occupants the panel's sequence view sources from. Called
    /// once at the head of [`run_for`](Self::run_for); exposed as the re-emit hook a
    /// future `ReloadSequences` would drive after rebuilding the payload.
    pub fn emit_sequence_registry(&mut self) {
        let _ = self.seq_registry_out.emit(&self.seq_registry);
    }

    /// The writer over the coordinator's command channel (`docs/messages.md` §4.3): the in-proc
    /// convenience for driving slots `Load`/`Start`/`Stop`/`Abort`/`Reset` — the host / CLI / a
    /// test [`emit`](MsgOut::emit)s [`SequenceCommand`]s the slots drain once per cycle (the same
    /// mechanism the uplink system uses, just an in-proc channel instead of a wire one). Address
    /// a slot by its **instance name** (`SequenceCommand::channel`) — the same key the wiring,
    /// the telemetry prefix, and the boot [`SequenceRegistry`] use.
    ///
    /// The channel has exactly **one** writer (the ring's writer claim enforces it), minted at
    /// `build()` and handed out here **once**: the first call returns it, every later call
    /// returns `None`. Take it once and hold it for the run — driving commands from one place is
    /// now structural, not a discipline note.
    ///
    /// Commands reach a slot only over an explicit `"coordinator" -> <slot>` edge
    /// (`connect … msg="SequenceCommand"`, A2): with no edge the handle is inert —
    /// visible in the wiring, diagnosable from the graph.
    pub fn control_handle(&mut self) -> Option<MsgOut<SequenceCommand>> {
        self.control_out.take()
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
        let rec = self.status_view.latest()?;
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
    ///
    /// # Panics
    ///
    /// Panics if called a second time on the same coordinator: a run *consumes* it —
    /// the async systems were moved into their (now torn-down) tasks and a transport's
    /// connection is gone — so a rerun would re-init every cyclic system over dead
    /// plumbing. Build a fresh `Coordinator` to run again.
    pub async fn run_for(&mut self, cycles: usize) {
        assert!(
            !self.started,
            "Coordinator::run_for called twice — a coordinator drives exactly one run \
             (its async systems/transports are consumed by the first); build a fresh \
             Coordinator to run again"
        );
        self.started = true;
        let tasks = self.start().await;
        // Emit the boot `SequenceRegistry` once, before the first cycle's events flow, so
        // a tap claimed after `build()` observes it ahead of any `SequenceChannelEvent`.
        if !self.seq_registry_emitted {
            self.emit_sequence_registry();
            self.seq_registry_emitted = true;
        }
        // The Wall pacing budget. Only computed under a `Wall` clock — `cycle_rate` is
        // documented ignored under `Simulated`, so an unusable rate must not panic
        // there; under `Wall` the rate was validated at `build()` (`InvalidCycleRate`),
        // so the conversion cannot panic.
        let budget = match self.config.clock {
            ClockMode::Wall => Duration::from_secs_f64(1.0 / self.config.cycle_rate),
            ClockMode::Simulated { .. } => Duration::ZERO,
        };
        // The epoch a `Simulated` clock advances from; unused under `Wall`.
        let epoch = Timestamp::now();
        for k in 0..cycles {
            let start = Instant::now();
            self.cycle += 1;
            // Publish progress for any out-of-loop observer (the CLI heartbeat).
            self.progress.store(self.cycle, Relaxed);
            // The per-cycle timestamp every system shares: wall time, or the
            // simulated clock at `epoch + k*dt` (coordinator.md §6, fix #5/#6).
            let now = match self.config.clock {
                ClockMode::Wall => Timestamp::now(),
                ClockMode::Simulated { dt } => simulated_now(epoch, dt, k as u64),
            };
            // Commands are drained per-slot at the head of each `step`: a slot's
            // declared `commands` fan-in reads exactly the producers explicitly edged
            // into it (A2) and filters by its instance name, so a command dispatches
            // the *same* cycle it lands — no coordinator-side command stage.
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
        // The uplink is now an ordinary async system (`docs/messages.md` §4.4) — spawned with
        // the rest above, no special reader task here.

        // Release the async tasks into their run loops.
        go_flag.store(true, Release);
        go.wake_all();
        tasks
    }

    /// Mirror fresh upstream records into each async system's private buffer
    /// (drop-on-full via overwrite), waking the async `recv` (coordinator.md §4.3).
    fn run_copy_ins(&mut self) {
        for c in &mut self.copy_ins {
            // A lap on the upstream output resyncs to the live edge and keeps
            // draining (the async consumer gets latest-wins) — the Resync policy.
            let writer = &mut c.writer;
            crate::port::drain_view(&mut c.upstream, &mut c.scratch, OnLap::Resync, |rec| {
                let _ = writer.try_write(rec);
            });
        }
    }

    /// Scan the slots; when the stopped set changes, refresh the status frame and
    /// log the change to coordinator health.
    fn update_status(&mut self, now: Timestamp) {
        let mut cur: Vec<StoppedSystem> = Vec::new();
        for slot in &self.cyclic {
            // Only a lapped/panicked stop is an error-stop; a runtime slot's
            // Empty/Loaded/Done states are not (the `stop_reason` projection).
            if let Some(reason) = slot.state().stop_reason() {
                cur.push(StoppedSystem {
                    name: slot.name(),
                    reason,
                });
            }
        }
        if !stopped_set_changed(&cur, &self.stopped) {
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
                    let (buf, len) = pack_name(name);
                    l.push(StoppedEntry {
                        reason: *reason,
                        len,
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
        // The uplink is an async system now; it tears down with the rest (its `AsyncTask`
        // drop guard cancels it if it is parked in `recv`).
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
