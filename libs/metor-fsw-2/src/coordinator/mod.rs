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
    Config, NoWake, Notifier, RingBuffer, View, WakeSource, Writer,
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
    Delivery, FanIn, Hz, PortConn, PortDesc, PortId, PortSchema, SystemDescriptor,
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

/// The default [`CoordinatorConfig::reader_slack`] (E8b: the knob is on the config;
/// this is only its default value).
const READER_SLACK: usize = 4;

/// Bounded window a teardown gives async tasks to exit cooperatively before their
/// `drop_guard` cancels them.
const JOIN_TIMEOUT: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// Public configuration / addressing / errors
// ---------------------------------------------------------------------------

/// Which clock drives the per-cycle timestamp (and whether the loop paces itself).
#[derive(Clone, Copy, Debug, Default)]
pub enum ClockMode {
    /// Wall-clock time: each cycle's `now` is `Timestamp::now()`, and the loop
    /// sleeps to hold `cycle_rate` (run-fast-then-wait). The default.
    #[default]
    Wall,
    /// A simulated clock: each cycle's `now` advances by `dt` from a start epoch and
    /// the loop does **not** sleep — it runs cycles as fast as possible. `dt` is the
    /// logical step (e.g. a physics integrator's), so a mission converges in fixed
    /// sim time regardless of how fast the host runs it (coordinator.md §6).
    Simulated { dt: Duration },
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
    /// Slack reader slots added on top of every buffer's computed fan-out (E8b),
    /// covering late taps claimed through the [`Registry`](crate::Registry) after
    /// `build()` — a db recorder, a debugger. v1 has no crash-slot reclamation, so
    /// each buffer's `max_readers` is fixed at build time (coordinator.md §2.3, Q7);
    /// exhausting the budget surfaces as a [`FullReaderTable`](metor_fsw_ring::FullReaderTable)
    /// error at the claim site (`RegistryEntry::view`), naming no panic. Default `4`.
    pub reader_slack: usize,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            cycle_rate: 100.0,
            default_depth: DEFAULT_DEPTH,
            clock: ClockMode::Wall,
            reader_slack: READER_SLACK,
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
// Slot state (the permanent hard-stop, coordinator.md §3.3/§3.4)
// ---------------------------------------------------------------------------

/// Why a cyclic slot hard-stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopReason {
    /// A dlopen'd system panicked inside the `.so` (the boundary caught it and
    /// returned [`FswStatus::Panicked`](crate::abi::FswStatus)).
    /// Only reachable for a [`DlSlot`](crate::dl); a static `CyclicRunner` cannot
    /// produce it (a panic there unwinds the host directly).
    Panicked,
}

impl StopReason {
    fn code(self) -> u8 {
        match self {
            // `1` was `LappedInput`, retired with the ring's overwrite mode.
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

/// Pack a name into a fixed [`NAME_CAP`] buffer + used length (truncating) — the
/// [`NAME_CAP`]-sized instantiation of the crate's one [`pack_str`] helper.
pub(crate) fn pack_name(name: &str) -> ([u8; NAME_CAP], u8) {
    crate::dynamic::pack_str::<NAME_CAP>(name)
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
/// variant payloads are debug-only (rendered via `Debug`, read by nothing) — the
/// `allow` covers exactly them; the variants themselves are matched
/// (`output_instances`).
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
struct RingEntry {
    /// Held for ownership only (the ports clone their own `Arc`s) — never read.
    #[allow(dead_code)]
    ring: RingBuffer,
    frame_id: ComponentId,
    role: BufferRole,
    /// The KDL/builder instance name of the system owning this buffer (the
    /// telemetry-sink prefix, wiring.md §6). `None` for coordinator-owned buffers.
    instance: Option<String>,
}

/// Owns every `RingBuffer`. Holding the canonical handle here keeps a
/// buffer alive longer than any port over it, regardless of teardown order.
struct RingTable {
    rings: Vec<RingEntry>,
}

// ---------------------------------------------------------------------------
// Async plumbing
// ---------------------------------------------------------------------------

/// A private-buffer copy-in job (coordinator.md §4.2/§4.3): mirrors the **newest**
/// upstream record (Snapshot semantics — the copy-in exists only for Snapshot
/// inputs) into the async system's private buffer, at most once per new upstream
/// commit. The record is borrowed in place off the upstream ring and written
/// through; no intermediate buffer.
struct CopyIn {
    upstream: View<NoWake, NoWake>,
    /// The private ring's sole writer: the matched data `Notifier` wakes the parked
    /// async `recv`; the space side is `NoWake` — a full private ring (the consumer
    /// is behind) drops this cycle's mirror rather than suspending the cycle loop.
    writer: Writer<Notifier, NoWake>,
    /// The upstream ring's `committed` at the last mirror, so an unchanged upstream
    /// (no new record) is skipped instead of re-waking the consumer with the same
    /// pinned record every cycle. `u64::MAX` = nothing mirrored yet.
    last_committed: u64,
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

/// One registered system: its type-erased registration, its **registered**
/// descriptor (what `build()` validates, sizes, and wires), and its instance name
/// (defaults to `System::NAME`; the wiring loader supplies a distinct KDL instance
/// name — wiring.md §6). One `Vec<Registration>` replaces the former four parallel
/// vectors (C5) — in particular the `kinds` vec, which always equaled `desc.kind`.
struct Registration {
    reg: Reg,
    desc: SystemDescriptor,
    name: String,
}

/// Registers systems and edges, then `build`s a ready [`Coordinator`]. This is the
/// surface the wiring loader targets (coordinator.md §2.1).
pub struct CoordinatorBuilder {
    config: CoordinatorConfig,
    systems: Vec<Registration>,
    /// Each registered edge `(producer, consumer, delayed)`. A `delayed` edge is an
    /// intentional one-cycle-delayed feedback edge, excluded from cycle detection.
    edges: Vec<(PortRef, PortRef, bool)>,
}

impl CoordinatorBuilder {
    fn new(config: CoordinatorConfig) -> Self {
        let mut b = Self {
            config,
            systems: Vec::new(),
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
        b.push_system(desc, COORDINATOR_INSTANCE.to_string(), Reg::Coordinator);
        b
    }

    /// Record one registration; the returned handle indexes `systems`.
    fn push_system(&mut self, desc: SystemDescriptor, name: String, reg: Reg) -> SystemHandle {
        let id = self.systems.len();
        self.systems.push(Registration { reg, desc, name });
        SystemHandle { id }
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
        &self.systems[handle.id].desc
    }

    /// Register a cyclic system under its type's `System::NAME` instance name; see
    /// [`add_cyclic_named`](Self::add_cyclic_named) to name the instance explicitly.
    pub fn add_cyclic<S, O>(&mut self, system: S) -> SystemHandle
    where
        S: CyclicSystem<Output = Out<O>> + 'static,
        O: SystemOutput + BindPorts + 'static,
        S::Input: BindPorts + 'static,
    {
        self.add_cyclic_named(<S as System>::NAME, system)
    }

    /// Register a cyclic system under an explicit instance name; returns a handle
    /// whose ports can be `connect`ed. The instance name disambiguates two instances
    /// of one system type at the telemetry sink (wiring.md §6).
    pub fn add_cyclic_named<S, O>(&mut self, name: impl Into<String>, system: S) -> SystemHandle
    where
        S: CyclicSystem<Output = Out<O>> + 'static,
        O: SystemOutput + BindPorts + 'static,
        S::Input: BindPorts + 'static,
    {
        self.push_system(
            <S as CyclicSystem>::descriptor(),
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

    /// Register an async system under an explicit instance name (wiring.md §6).
    pub fn add_async_named<S>(&mut self, name: impl Into<String>, system: S) -> SystemHandle
    where
        S: AsyncSystem + 'static,
        S::Input: BindPorts + 'static,
        S::Output: BindPorts + 'static,
    {
        self.push_system(
            <S as AsyncSystem>::descriptor(),
            name.into(),
            Reg::Async(Box::new(AsyncReg { system })),
        )
    }

    /// Register the telemetry downlink (telemetry.md §8). This is an ordinary
    /// [`add_cyclic_named`](Self::add_cyclic_named) of a [`TelemetrySystem`] under the
    /// instance name `"telemetry"`, registered **last** so its end-of-cycle snapshot
    /// observes every other system's fresh output. The downlink's `AllOutputs` receive-all
    /// port is what reserves it a reader slot on every buffer — `build()` derives that budget
    /// by counting `ReceiveAll` capabilities, so no manual bookkeeping is needed here.
    ///
    /// **Init-time emit gap (B9)**: the downlink claims its read views in its own
    /// `init`, which runs *after* earlier-registered systems' `init`s — a frame or
    /// message a system emits during `init` is therefore **not** downlinked (the
    /// view starts at the live edge past it). Values that must reach the ground
    /// should be (re-)published from the first `execute`; the coordinator's own boot
    /// `SequenceRegistry` deliberately emits at the head of `run_for`, after every
    /// `init`, for exactly this reason.
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
        let mut desc = loaded.descriptor().clone();
        // Dl systems are cyclic-only: the registered kind is pinned here (as the
        // former parallel `kinds` vec hard-coded it), never trusted from the
        // decoded wire mirror.
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

    /// Register a runtime-swappable **slot** (sequences-slots.md §6): a fixed position in
    /// the cyclic call chain whose occupant the host `Load`s/`Start`s/`Stop`s/`Abort`s/
    /// `Reset`s/`Unload`s at runtime. `allowed` is the pre-opened candidate set — each
    /// [`AllowedOccupant`] a sequence occupant (its `Load` name, opened `DlSystem`, and
    /// postcard params — E8e: the public type, not a tuple). `initial` optionally
    /// applies one at startup.
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
        allowed: Vec<AllowedOccupant>,
        initial: Option<InitialOccupant>,
    ) -> SystemHandle {
        assert!(
            !allowed.is_empty(),
            "a slot needs at least one allowed occupant"
        );
        let base = allowed[0].system.descriptor().clone();
        // Every allowed occupant must share the contract (the slot sizes/validates to the
        // first occupant's descriptor). v1 requires a shared shape (mutual subset).
        for occ in &allowed[1..] {
            let d = occ.system.descriptor();
            let ports_match = |a: &[PortDesc], b: &[PortDesc]| {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|(x, y)| compatible(x, y) && compatible(y, x))
            };
            assert!(
                ports_match(&d.inputs, &base.inputs) && ports_match(&d.outputs, &base.outputs),
                "allowed occupant {:?} is incompatible with the slot contract \
                 (derived from {:?})",
                occ.name,
                allowed[0].name
            );
        }
        // W1b: the builder path validates the initial occupant against the allowed
        // set at build, like the resolve path's `UnknownInitialOccupant` — a typo
        // would otherwise surface only as a runtime `Failed` event.
        if let Some(init) = &initial {
            assert!(
                allowed.iter().any(|a| a.name == init.occupant),
                "initial occupant {:?} is not in the allowed set ({})",
                init.occupant,
                allowed
                    .iter()
                    .map(|a| a.name.as_str())
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

        self.push_system(
            registered,
            name,
            Reg::Slot(SlotReg {
                allowed,
                initial,
                n_occ_inputs,
                n_occ_outputs,
            }),
        )
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

    /// Validate the graph, size and allocate every ring, bind ports, auto-provision
    /// health/log buffers, and return a ready coordinator (coordinator.md §2).
    ///
    /// One orchestrator over named passes (C2): each pass owns one former section of
    /// the monolith and hands its product to the next — validation, edge resolution,
    /// fan-out counting, ring allocation, registry freeze, copy-in planning, bind.
    pub fn build(self) -> Result<Coordinator, WireError> {
        self.validate_cycle_rate()?;
        self.validate_receive_all_last()?;
        self.validate_slot_name_caps()?;
        self.validate_port_axes()?;
        let cons_edges = self.resolve_edges()?;
        let fan_out = self.count_fan_out(&cons_edges);
        let mut alloc = self.alloc_rings(&fan_out);
        let seq_registry = self.seq_registry_payload();
        let registry = freeze_registry(std::mem::take(&mut alloc.reg_entries))?;
        let mut plumbing = self.plan_copy_ins(&cons_edges, &mut alloc);
        let Self {
            config, systems, ..
        } = self;
        let BoundSystems {
            cyclic,
            pending_async,
            coord,
        } = bind_systems(systems, &cons_edges, &alloc, &mut plumbing, &registry);

        Ok(Coordinator {
            config,
            cyclic,
            pending_async,
            copy_ins: plumbing.copy_ins,
            coord_health: coord.health,
            status_out: coord.status_out,
            status_view: coord.status_view,
            stopped: Vec::new(),
            stopped_scratch: Vec::new(),
            cycle: 0,
            progress: Arc::new(AtomicU64::new(0)),
            registry,
            control_out: Some(coord.control_out),
            seq_registry_out: coord.seq_registry_out,
            seq_registry,
            seq_registry_emitted: false,
            started: false,
            // Declared last so the canonical ring handles drop after every port.
            rings: alloc.table,
        })
    }

    // -----------------------------------------------------------------------
    // build() passes (C2) — each one former section of the monolith, in order.
    // -----------------------------------------------------------------------

    /// A Wall clock turns `cycle_rate` into the per-cycle pacing budget in `run_for`;
    /// reject an unusable rate here so the failure is a build-time `WireError`, not a
    /// `Duration::from_secs_f64` panic mid-run. A `Simulated` clock ignores the rate
    /// (coordinator.md §6), so it is deliberately not validated there.
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

    /// Receive-all (telemetry) systems register last (A11(b): enforced). The
    /// downlink's end-of-cycle snapshot only observes systems stepping *before* it,
    /// so a cyclic system registered after it would telemeter one cycle stale.
    /// Enforced, not silently reordered: reordering registrations would change the
    /// step order the stale-edge diagnostics validate. Async systems are exempt
    /// (they run off their own task, not the registration-ordered step loop).
    fn validate_receive_all_last(&self) -> Result<(), WireError> {
        let mut first_receive_all: Option<usize> = None;
        for (s, sys) in self.systems.iter().enumerate() {
            if sys.desc.kind != SystemKind::Cyclic {
                continue;
            }
            let has_receive_all = sys.desc.capabilities.contains(&crate::Capability::ReceiveAll);
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
    /// `SequenceCommand` addresses a slot by its instance name, and the same name
    /// packs into fixed-size status frames; > NAME_CAP would telemeter truncated
    /// while addressing untruncated, so it is a build error, never a truncation.
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

    /// Per-descriptor axis validation (no edges needed).
    /// FanIn::Many × Delivery::Snapshot: latest-wins across producers is
    /// ill-defined.
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

    /// Validate every edge and build the ONE connection map:
    /// `(cons_id, in_idx) -> [(prod_id, out_idx)]` for EVERY input — a FanIn::One
    /// input holds exactly one entry (enforced here), a FanIn::Many input zero or
    /// more. Every rule branches on a descriptor axis, never on frame-vs-message.
    /// Also runs the graph-shape checks over the map: every feedback loop must be
    /// broken by a `connect_delayed`, registration order must agree with the
    /// dataflow, and every FanIn::One input must be connected.
    fn resolve_edges(&self) -> Result<ConsEdges, WireError> {
        let n = self.systems.len();
        let mut cons_edges: ConsEdges = HashMap::new();
        // System-level adjacency over the NON-delayed SNAPSHOT edges only, for cycle
        // detection: a remaining cycle is an unbroken feedback loop. Log edges are
        // excluded — a log is a decoupled event/command stream, not a same-cycle
        // dependency (§3.6).
        let mut forward_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (p, c, delayed) in &self.edges {
            let prod = &self.systems[p.system.id].desc;
            let cons = &self.systems[c.system.id].desc;
            let out_idx = prod
                .outputs
                .iter()
                .position(|d| d.id == p.port)
                .ok_or(WireError::UnknownPort {
                    system: p.system.id,
                    port: p.port,
                })?;
            let in_idx = cons
                .inputs
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
            // A host-connected input's counterpart is held by the system's runner
            // (the slot's cancel writer, a self-tap over its own output) — an edge
            // into it is rejected (A3). Host *outputs* keep accepting consumer edges:
            // the coordinator's `commands` channel is exactly a Host output slots
            // read over explicit edges.
            if in_desc.conn != PortConn::Edge {
                return Err(WireError::HostPort {
                    system: cons.name,
                    port: c.port,
                });
            }
            // `delayed` marks a one-cycle-late snapshot sample; on a Log input it is
            // meaningless and rejected instead of silently ignored (A7).
            if *delayed && in_desc.delivery == Delivery::Log {
                return Err(WireError::DelayedLogEdge {
                    producer: prod.name,
                    consumer: cons.name,
                    port: c.port,
                });
            }
            let producers = cons_edges.entry((c.system.id, in_idx)).or_default();
            match in_desc.fan_in {
                // Exactly one edge per input (the frame doctrine).
                FanIn::One => {
                    if !producers.is_empty() {
                        return Err(WireError::DoubleConnect {
                            system: cons.name,
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
            // any other loop — the DFS reports it as a one-member `FeedbackCycle`.
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
            let prod = &self.systems[p.system.id].desc;
            let cons = &self.systems[c.system.id].desc;
            let in_delivery = cons.inputs.iter().find(|d| d.id == c.port).map(|d| d.delivery);
            if in_delivery != Some(Delivery::Snapshot) {
                continue;
            }
            let both_cyclic =
                prod.kind == SystemKind::Cyclic && cons.kind == SystemKind::Cyclic;
            if both_cyclic && c.system.id < p.system.id {
                return Err(WireError::StaleFrameEdge {
                    producer: prod.name,
                    consumer: cons.name,
                    port: c.port,
                });
            }
        }

        // --- Input coverage: a FanIn::One input must be connected exactly once ---
        // (exactly-once is the edge pass above; existence is here). A FanIn::Many
        // input may have zero producers (fan-in is optional, §3.2); a non-Edge input
        // is fed by its runner, never an edge (A3).
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

    /// Fan-out per output port: one uniform count over the one connection map. A
    /// declared self-tap (A3) is one more reader on the system's *own* output —
    /// counted here so the budget is explicit, not slack-covered.
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

    /// Allocate one buffer per output port (incl. health/log) plus a dedicated ring
    /// per Host-connected input (A3), collecting the build-order registry entries —
    /// ONE list over every registered buffer, frames and message channels alike
    /// (telemetry.md §2). Each `AllOutputs` receive-all capability
    /// (`docs/message-wiring.md` §4) is an extra fan-out reader on *every* buffer, so
    /// every ring's `max_readers` includes it — self-derived from the declared
    /// `ReceiveAll` capabilities, no manual `add_telemetry` bookkeeping.
    fn alloc_rings(&self, fan_out: &HashMap<(usize, usize), usize>) -> RingAlloc {
        let depth = self.config.default_depth;
        let slack = self.config.reader_slack;
        let n_reg = self
            .systems
            .iter()
            .flat_map(|sys| sys.desc.capabilities.iter())
            .filter(|c| **c == crate::Capability::ReceiveAll)
            .count();

        let mut alloc = RingAlloc {
            table: RingTable { rings: Vec::new() },
            output_rings: Vec::with_capacity(self.systems.len()),
            host_input_rings: HashMap::new(),
            reg_entries: Vec::new(),
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
                // ONE sizing path (depth by delivery — `alloc_ring`); only the
                // registry-entry shape still splits on the schema. Command channels
                // are ordinary outputs here: a slot reads a producer only over an
                // explicit edge, so the edge fan-out counts its readers exactly (A2).
                let ring = alloc_ring(port.delivery, port.max_size, depth, readers);
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
                        // Registered like any buffer; the downlink / `AllOutputs`
                        // taps it unless the port opted out via
                        // `telemetered = false` (e.g. a command channel, §6.4).
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

        // --- Dedicated rings for Host-connected inputs (A3) ----------------
        // A Host input's counterpart is its runner's writer (the slot's cancel
        // frame), so it gets its own ring instead of a producer edge: the occupant
        // attaches one read `View` per Load (released on each Stop/Reset/Unload), so
        // 1 reader slot + slack covers the reload cycle. No registry entry — it is
        // inbound control, not an output. SelfTap inputs allocate nothing (they view
        // the system's own output, already counted in `fan_out`).
        for (sid, sys) in self.systems.iter().enumerate() {
            for (in_idx, port) in sys.desc.inputs.iter().enumerate() {
                if port.conn != PortConn::Host {
                    continue;
                }
                let ring = alloc_ring(port.delivery, port.max_size, depth, 1 + slack);
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

        alloc
    }

    /// The boot `SequenceRegistry` payload: one spec per slot, keyed by the slot's
    /// **instance name** — the channel's wire address (`docs/messages.md` §5); there
    /// is no build-order channel id.
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

    /// Private copy-in buffers for async inputs, keyed on the delivery axis (§2.3):
    /// an async system cannot be step-gated, so an async SNAPSHOT input is decoupled
    /// through a private latest-wins copy-in ring (which also supplies the matched
    /// data `Notifier` the async `recv` parks on). Log inputs use a direct fan-in
    /// multi-view (§3.3) — an every-record log the consumer poll-drains, no copy-in.
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
                // Only edge-connected Snapshot inputs are copy-in decoupled; a
                // Host/SelfTap input is fed by its runner, not a producer edge (A3).
                if port.delivery == Delivery::Log || port.conn != PortConn::Edge {
                    continue;
                }
                let (prod_id, out_idx) = cons_edges[&(sid, in_idx)][0];
                let private = alloc_ring(port.delivery, port.max_size, depth, 1 + slack);
                let data = Notifier::default();
                // Only the matched DATA notifier is load-bearing (it wakes the
                // parked async `recv`); the writer's space side is `NoWake` — the
                // copy-in uses `try_write` and skips a full private ring.
                // Invariant: each private copy-in ring is created here and gets
                // this one writer, so the claim is always free.
                let writer = private
                    .writer(data.clone(), NoWake)
                    .expect("private copy-in ring has exactly one writer");
                let upstream = alloc.output_rings[prod_id][out_idx]
                    .view(NoWake, NoWake)
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
// build() products + the bind pass (C2)
// ---------------------------------------------------------------------------

/// The ONE connection map: `(consumer, input-index)` → the producer endpoints
/// explicitly wired into it. Product of [`CoordinatorBuilder::resolve_edges`];
/// consumed by fan-out counting, copy-in planning, and the bind pass.
type ConsEdges = HashMap<(usize, usize), Vec<(usize, usize)>>;

/// The ring-allocation pass product: the canonical owning [`RingTable`], one buffer
/// row per system's outputs, the dedicated Host-input rings, and the build-order
/// registry entries (drained by [`freeze_registry`]).
struct RingAlloc {
    table: RingTable,
    output_rings: Vec<Vec<RingBuffer>>,
    host_input_rings: HashMap<(usize, usize), RingBuffer>,
    reg_entries: Vec<RegistryEntry>,
}

/// The copy-in planning product: each async Snapshot input's private ring + matched
/// data notifier, the per-system wake lists (for teardown), and the copy-in jobs.
struct AsyncPlumbing {
    private_inputs: HashMap<(usize, usize), (RingBuffer, Notifier)>,
    async_wakes: Vec<Vec<Notifier>>,
    copy_ins: Vec<CopyIn>,
}

/// The coordinator's own (#0) bound ports, wrapped by [`bind_coordinator`].
struct CoordinatorPorts {
    health: HealthPort,
    status_out: Output<CoordinatorStatus>,
    status_view: Input<CoordinatorStatus>,
    seq_registry_out: MsgOut<SequenceRegistry>,
    control_out: MsgOut<SequenceCommand>,
}

/// The bind pass product: every cyclic slot, every pending async system, and the
/// coordinator's own ports.
struct BoundSystems {
    cyclic: Vec<Box<dyn CyclicSlot>>,
    pending_async: Vec<PendingAsync>,
    coord: CoordinatorPorts,
}

/// One keyspace over frames and channels: a same-instance name collision between a
/// frame and a channel (both `"<instance>.<name>"`) is detectable instead of
/// shadowing. Freezes the ONE registry; every consumer's bind pulls this handle.
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

/// Bind every system's ports over the allocated rings — the pass that consumes the
/// registrations. Each arm mirrors one registration kind; the static (host-side
/// host) arm builds typed `BoundPort`s and walks them with a [`Binder`].
fn bind_systems(
    systems: Vec<Registration>,
    cons_edges: &ConsEdges,
    alloc: &RingAlloc,
    plumbing: &mut AsyncPlumbing,
    registry: &Arc<Registry>,
) -> BoundSystems {
    let mut cyclic: Vec<Box<dyn CyclicSlot>> = Vec::new();
    let mut pending_async: Vec<PendingAsync> = Vec::new();
    // The coordinator's own (#0) ports, wrapped by its bind arm below and
    // unwrapped after the loop (`Reg::Coordinator` is always registered first).
    let mut coord: Option<CoordinatorPorts> = None;
    for (id, registration) in systems.into_iter().enumerate() {
        let Registration { reg, desc, .. } = registration;
        match reg {
            Reg::Coordinator => coord = Some(bind_coordinator(id, &desc, &alloc.output_rings)),
            Reg::Dl(dl) => cyclic.push(Box::new(bind_dl(
                id,
                dl,
                &desc,
                cons_edges,
                &alloc.output_rings,
            ))),
            Reg::Slot(slot_reg) => cyclic.push(Box::new(bind_slot(
                id, slot_reg, &desc, cons_edges, alloc,
            ))),
            // The static (host-side) path: build typed `BoundPort`s and
            // walk them with a `Binder`.
            reg => {
                // Outputs: default wakes, the system's own buffers. Capabilities
                // never appear here — they live on `desc.capabilities`, not in the
                // port lists, so the positional cursor covers exactly the wired
                // ports (`AllOutputs::bind` pulls the registry instead of consuming
                // a cursor position).
                let outs: Vec<BoundPort> = (0..desc.outputs.len())
                    .map(|out_idx| BoundPort::new(alloc.output_rings[id][out_idx].clone()))
                    .collect();
                // Inputs, in `descriptors()` order, chosen by the FAN-IN axis. A
                // `One` input: cyclic consumers view the producer's output
                // directly, async consumers view their private copy-in buffer with
                // the matched data wake. A `Many` input: a direct multi-view over
                // every producer ring wired to it (fan-in, §3.3), NoWake — a
                // best-effort log the consumer poll-drains (no copy-in, cyclic or
                // async).
                let ins: Vec<BoundInput> = (0..desc.inputs.len())
                    .map(|in_idx| match desc.inputs[in_idx].fan_in {
                        FanIn::One => {
                            let (prod_id, out_idx) = cons_edges[&(id, in_idx)][0];
                            // The copy-in pass keyed on (Async, Snapshot); anything
                            // it decoupled binds the private ring + matched wake,
                            // everything else views the producer directly.
                            let port = match plumbing.private_inputs.get(&(id, in_idx)) {
                                Some((ring, data)) => {
                                    BoundPort::matched(ring.clone(), Box::new(data.clone()))
                                }
                                None => {
                                    BoundPort::new(alloc.output_rings[prod_id][out_idx].clone())
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
                                            BoundPort::new(
                                                alloc.output_rings[prod_id][out_idx].clone(),
                                            )
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
                        wake_on_stop: std::mem::take(&mut plumbing.async_wakes[id]),
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

    BoundSystems {
        cyclic,
        pending_async,
        // Always registered by CoordinatorBuilder::new, so the unwrap is structural.
        coord: coord.expect("coordinator #0 bound its ports"),
    }
}

/// The coordinator's own bundle (§2.6): a marker registration — not a cyclic slot
/// (the coordinator IS the loop). Its declared Host outputs were allocated and
/// registered by the uniform passes; wrap the writers into the coordinator's ports
/// here, single-writer by construction, and claim the status SelfTap view
/// (`read_status`).
fn bind_coordinator(
    id: usize,
    desc: &SystemDescriptor,
    output_rings: &[Vec<RingBuffer>],
) -> CoordinatorPorts {
    let out_idx = |pid: PortId| {
        desc.outputs
            .iter()
            .position(|p| p.id == pid)
            .expect("the coordinator #0 bundle declares this output")
    };
    let health_ring = &output_rings[id][out_idx(PortId::Component(SystemHealth::FRAME_ID))];
    let log_ring = &output_rings[id][out_idx(PortId::Component(SystemLog::FRAME_ID))];
    let health = HealthPort::new(
        slot_writer::<SystemHealth>(health_ring),
        slot_writer::<SystemLog>(log_ring),
    );
    let status_idx = out_idx(PortId::Component(CoordinatorStatus::FRAME_ID));
    let status_out = slot_writer::<CoordinatorStatus>(&output_rings[id][status_idx]);
    // The declared SelfTap over the coordinator's own status output (+1 fan-out
    // counted at sizing).
    let status_view = Input::new(
        output_rings[id][status_idx]
            .view(NoWake, NoWake)
            .expect("status self-tap reader (fan-out sized)"),
    );
    let seq_registry_out = owned_writer::<SequenceRegistry>(
        &output_rings[id][out_idx(PortId::Packet(SequenceRegistry::ID))],
    );
    let control_out = owned_writer::<SequenceCommand>(
        &output_rings[id][out_idx(PortId::Packet(SequenceCommand::ID))],
    );
    CoordinatorPorts {
        health,
        status_out,
        status_view,
        seq_registry_out,
        control_out,
    }
}

/// A dlopen'd system binds over **raw** `FswRing` regions, not typed `BoundPort`s:
/// gather the same per-port rings the coordinator allocated (outputs = this system's
/// own buffers; inputs = views into the upstream producers' outputs — the
/// cyclic-consumer path), as `(base, len, role)` handles in `descriptors()` order,
/// and hand them to a `DlSlot`. Sizing, allocation, validation, and the registry
/// entry are identical to a static system's.
fn bind_dl(
    id: usize,
    dl: DlReg,
    desc: &SystemDescriptor,
    cons_edges: &ConsEdges,
    output_rings: &[Vec<RingBuffer>],
) -> crate::dl::DlSlot {
    use crate::abi::{FswRing, ROLE_INPUT, ROLE_OUTPUT};
    let outputs: Vec<FswRing> = (0..desc.outputs.len())
        .map(|out_idx| {
            let (base, len) = output_rings[id][out_idx].region();
            FswRing {
                base,
                len,
                role: ROLE_OUTPUT,
            }
        })
        .collect();
    let inputs: Vec<FswRing> = (0..desc.inputs.len())
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
    // SAFETY: every region named here is a `RingTable`-owned ring that outlives the
    // slot — the coordinator drops `cyclic` (this slot, whose `Drop` calls
    // `fsw_destroy`) before `rings`. The `DlSystem` handle drops right after; the
    // slot keeps its own `Arc<Library>`.
    unsafe { dl.system.make_slot(&dl.params, inputs, outputs, desc.name) }
}

/// A runtime slot: gather the same per-port regions as the dl arm, but locate the
/// runner's tail ports by their declared shape and hand the runner the
/// control/status writers. No occupant is created here — only `init`/`Load`
/// (runtime) does.
fn bind_slot(
    id: usize,
    slot_reg: SlotReg,
    desc: &SystemDescriptor,
    cons_edges: &ConsEdges,
    alloc: &RingAlloc,
) -> SlotRunner {
    use crate::abi::{FswRing, ROLE_INPUT, ROLE_OUTPUT};
    let SlotReg {
        allowed,
        initial,
        n_occ_inputs,
        n_occ_outputs,
    } = slot_reg;
    let output_rings = &alloc.output_rings;
    // The prefix/tail invariant (§2.2): the occupant's ports are the prefix of each
    // registered list, in the occupant descriptor's own order — so the occupant
    // `FswRing` arrays are a straight prefix map (Edge inputs view their producers;
    // the Host `SlotControlIn` input its dedicated ring) and the occupant-side
    // positional bind contract (the dl ABI) is untouched.
    let inputs: Vec<FswRing> = (0..n_occ_inputs)
        .map(|in_idx| {
            let (base, len) = match desc.inputs[in_idx].conn {
                PortConn::Edge => {
                    let (prod_id, out_idx) = cons_edges[&(id, in_idx)][0];
                    output_rings[prod_id][out_idx].region()
                }
                PortConn::Host => alloc.host_input_rings[&(id, in_idx)].region(),
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
    // Occupant outputs = the prefix of the slot's own buffers (user outputs +
    // SequenceStatus + health + log, in descriptor order).
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

    // --- The runner's tail ports, located by their declared shape --------------
    // Host cancel writer over the SlotControlIn input's dedicated ring.
    let control_in_idx = desc.inputs[..n_occ_inputs]
        .iter()
        .position(|p| p.conn == PortConn::Host)
        .expect("a slot declares its Host SlotControlIn input");
    let control = slot_writer::<SlotControlIn>(&alloc.host_input_rings[&(id, control_in_idx)]);
    // The slot's command fan-in: one view per producer explicitly edged into the
    // declared `commands` input (A2 — no type-keyed broadcast; zero edges is a
    // legal, command-less slot). The `SlotRunner` drains + filters by its instance
    // name each step.
    let cmd_in_idx = desc
        .inputs
        .iter()
        .position(|p| p.conn == PortConn::Edge && p.id == PortId::Packet(SequenceCommand::ID))
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
    // The declared self-tap over the occupant's own SequenceStatus output (+1
    // fan-out counted at sizing) — Progress + outcome.
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
    // Host writers over the runner's declared output tail: SlotStatus + the
    // "sequences" events channel (real output indices — no off-by-the-end
    // BufferRole, no side allocation).
    let status_out_idx = desc
        .outputs
        .iter()
        .position(|o| o.id == PortId::Component(SlotStatus::FRAME_ID))
        .expect("a slot declares its Host SlotStatus output");
    let status_out = slot_writer::<SlotStatus>(&output_rings[id][status_out_idx]);
    let events_out_idx = desc
        .outputs
        .iter()
        .position(|o| o.id == PortId::Packet(SequenceChannelEvent::ID))
        .expect("a slot declares its Host events output");
    let events = owned_writer::<SequenceChannelEvent>(&output_rings[id][events_out_idx]);

    SlotRunner::new(
        desc.name,
        allowed,
        initial,
        inputs,
        outputs,
        control,
        status_out,
        events,
        seq_status,
        commands,
    )
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
/// slow tap).
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

/// Mint the single [`MsgOut`] writer over a coordinator-owned ring — the
/// [`slot_writer`] analogue for the message channel, exactly how the coordinator
/// mints its own `status_out`/`control` writers. Called exactly once per ring at
/// build (the region's writer claim enforces it).
fn owned_writer<M: Msg>(ring: &RingBuffer) -> MsgOut<M> {
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

/// The synthetic instance prefix coordinator-owned buffers downlink under
/// (telemetry.md §6): they have no system instance, so their qualified key is
/// `coordinator.health` / `coordinator.log` / `coordinator.coordinator_status`.
const COORDINATOR_INSTANCE: &str = "coordinator";

/// Build a [`RegistryEntry`] for one buffer: compute the instance-qualified key and
/// the prefixed announce vtable+metadata once (telemetry.md §2.1/§6), capturing a
/// clone of the ring as the read source.
fn registry_entry(instance: &str, port: &PortDesc, ring: RingBuffer) -> RegistryEntry {
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
    /// Scratch for `update_status`'s per-cycle scan, swapped with `stopped` on a
    /// change — no per-cycle allocation in the hot loop (S5).
    stopped_scratch: Vec<StoppedSystem>,
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

    /// Mirror the newest upstream record into each async system's private buffer,
    /// waking the async `recv` (coordinator.md §4.3). Snapshot semantics: older
    /// unread upstream records are consumed on the way (freed for the producer) and
    /// only the newest is mirrored, at most once per new upstream commit. A full
    /// private ring (the consumer is behind) skips this cycle's mirror — the next
    /// cycle retries with whatever is newest then.
    fn run_copy_ins(&mut self) {
        for c in &mut self.copy_ins {
            // Skip untouched upstreams: `committed` moves iff a record landed on
            // this ring, so this also keeps the pinned newest record from being
            // re-mirrored (and the consumer re-woken) every cycle.
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

    /// Scan the slots; when the stopped set changes, refresh the status frame and
    /// log the change to coordinator health. The scan fills a retained scratch and
    /// swaps it with `stopped` on a change — no per-cycle allocation (S5).
    fn update_status(&mut self, now: Timestamp) {
        self.stopped_scratch.clear();
        for slot in &self.cyclic {
            // Only a panicked stop is an error-stop; a runtime slot's
            // Empty/Loaded/Done states are not (the `stop_reason` projection).
            if let Some(reason) = slot.state().stop_reason() {
                self.stopped_scratch.push(StoppedSystem {
                    name: slot.name(),
                    reason,
                });
            }
        }
        if !stopped_set_changed(&self.stopped_scratch, &self.stopped) {
            return;
        }
        core::mem::swap(&mut self.stopped, &mut self.stopped_scratch);
        self.publish_status(now);
        for i in 0..self.stopped.len() {
            let name = self.stopped[i].name;
            self.coord_health.error("system_stopped");
            self.coord_health.log(Level::Warn, name);
        }
        self.coord_health.end_cycle(now, 0);
    }

    fn publish_status(&mut self, now: Timestamp) {
        let frame = CoordinatorStatus {
            timestamp: now,
            cycle: self.cycle,
            stopped_count: self.stopped.len() as u64,
            stopped: FrameList::EMPTY,
        };
        // Split borrows: the writer takes `status_out`, the closure reads `stopped`
        // — no intermediate entries Vec (S5).
        let stopped = &self.stopped;
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
