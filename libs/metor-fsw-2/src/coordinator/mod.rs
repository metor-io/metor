//! The cyclic run loop.
//!
//! This module is the runtime: a ready [`Coordinator`] and the supervision it
//! runs. The graph construction that produces one — collecting systems and
//! edges, validating, sizing, binding — lives in [`init`]; a `Coordinator`
//! arrives already wired, from the wiring front-end
//! ([`resolve`](crate::wiring::resolve)) via [`init::InitGraph::build`].
//! [`Coordinator::run_for`] drives the lifecycle: spawn the async systems, init
//! everything behind a barrier, step the cyclic systems once per cycle, run the
//! async copy-in mirror, publish coordinator-level health and a status frame,
//! and tear it all down.
//!
//! Cyclic systems step in registration order, once per cycle; the build-time
//! passes ([`init`]) reject any wiring whose dataflow disagrees with that
//! order, so the loop here never has to reason about staleness. Async systems
//! run on their own tasks, off the cycle clock, and observe their snapshot
//! inputs through the post-step copy-in mirror.

use core::mem::offset_of;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::time::{Duration, Instant};

use metor_fsw_ring::{NoWake, Notifier, RingBuffer, View, Writer};
use metor_proto::types::{ComponentId, Timestamp};
use metor_proto_wkt::{ReloadSequences, SequenceCommand, SequenceRegistry, WiringManifest};
use stellarator::JoinHandle;
use stellarator::sync::WaitQueue;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::DEFAULT_DEPTH;
use crate::descriptor::{Hz, PortId};
use crate::dynamic::FrameList;
use crate::health::{HealthPort, Level};
use crate::message::{MsgIn, MsgOut};
use crate::port::Output;
use crate::proc::session::SessionDir;
use crate::registry::Registry;
use crate::system::{AsyncContext, AsyncSystem};

mod bind;
mod error;
pub(crate) mod init;
pub(crate) mod slot;
mod status;

pub use error::WireError;
pub(crate) use slot::validate_slot_spec;
pub use slot::{AllowedOccupant, InitialOccupant, OccupantBacking, SlotConfigError, SlotStatus};
pub(crate) use status::{CyclicSlot, pack_name};
pub use status::{NAME_CAP, SlotState, StopReason, StoppedSystem, WorkerRunState, WorkerStatus};

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

/// An opaque index naming one registered system. The graph's `add_*`
/// conveniences return it, and a [`PortRef`] embeds it to address that system's
/// ports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SystemHandle {
    id: usize,
}

/// A `(system, port)` pair addressing one port for wiring, both halves taken
/// from the system's registered [`SystemDescriptor`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PortRef {
    pub system: SystemHandle,
    pub port: PortId,
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
pub(crate) struct RingTable {
    rings: Vec<RingEntry>,
}

// ---------------------------------------------------------------------------
// Async plumbing
// ---------------------------------------------------------------------------

/// One mirroring job that copies the newest upstream record into an async
/// system's private buffer, at most once per new upstream commit. It exists
/// only for snapshot inputs; the record is borrowed in place off the upstream
/// ring and written through, with no intermediate buffer.
pub(crate) struct CopyIn {
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
pub(crate) struct LaunchCtx {
    cancel: stellarator::util::CancelToken,
    ready: Arc<WaitQueue>,
    ready_count: Arc<AtomicUsize>,
    go: Arc<WaitQueue>,
    go_flag: Arc<AtomicBool>,
}

/// Spawns a bound async system onto its own task, exactly once. Erased so the
/// coordinator can hold a heterogeneous set.
pub(crate) trait AsyncLauncher {
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
            let context = AsyncContext { cancel: ctx.cancel };
            me.system.run(&context, &mut me.input, &mut me.output).await;
            me.system.shutdown(&mut me.output);
        })
    }
}

/// A spawned async task plus the handles the coordinator drives its lifecycle
/// with. The `drop_guard` cancels the task if it does not exit cooperatively
/// (and when a `Coordinator` is dropped mid-run).
struct AsyncTask {
    name: String,
    handle: Option<JoinHandle<()>>,
    cancel: stellarator::util::CancelToken,
}

impl Drop for AsyncTask {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = &self.handle {
            let _ = handle.0.cancel();
        }
    }
}

/// A bound async system awaiting `run` (built at `build`, spawned at `run`).
struct PendingAsync {
    name: String,
    launcher: Box<dyn AsyncLauncher>,
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
/// is checked and the timestamp saturates rather than wrapping if the run
/// exceeds the representable timestamp range.
fn simulated_now(epoch: Timestamp, dt: Duration, k: u64) -> Timestamp {
    let delta = dt
        .as_nanos()
        .checked_mul(k as u128)
        .map(|nanos| nanos / 1_000)
        .unwrap_or(u128::MAX);
    let room = (i128::from(i64::MAX) - i128::from(epoch.0)) as u128;
    if delta > room {
        Timestamp(i64::MAX)
    } else {
        Timestamp(epoch.0 + delta as i64)
    }
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

/// The coordinator's own announce/control channels on its #0 bundle: the boot
/// [`SequenceRegistry`] and [`WiringManifest`] broadcast once at the head of a
/// run (and re-fired on a [`ReloadSequences`] request so a late consumer
/// resyncs), the take-once operator command writer, and the reload request
/// fan-in drained each cycle. The two `_emitted` latches make the boot
/// emission idempotent; the manifest channel is absent unless a front-end set
/// one.
struct CoordChannels {
    /// The single writer over the coordinator's `commands` output, handed out
    /// once by [`take_control`](Self::take_control) (see
    /// [`Coordinator::control_handle`] for the take-once contract).
    control_out: Option<MsgOut<SequenceCommand>>,
    /// The sole writer of the coordinator's boot-`SequenceRegistry` channel.
    seq_registry_out: MsgOut<SequenceRegistry>,
    /// The prebuilt boot [`SequenceRegistry`] payload (the slots plus their
    /// allowed occupants), emitted once at the head of a run.
    seq_registry: SequenceRegistry,
    seq_registry_emitted: bool,
    /// The sole writer of the coordinator's `wiring` channel, present only when
    /// a front-end supplied a manifest.
    wiring_out: Option<MsgOut<WiringManifest>>,
    /// The full mission IR to broadcast on the `wiring` channel; `None` mirrors
    /// `wiring_out`.
    wiring_manifest: Option<WiringManifest>,
    wiring_emitted: bool,
    /// The [`ReloadSequences`] fan-in, drained each cycle: any request re-emits
    /// the registry and manifest, so a consumer that connected after boot can
    /// recover the channel list on demand.
    reload_in: MsgIn<ReloadSequences>,
}

impl CoordChannels {
    /// Emit the boot [`SequenceRegistry`] on the coordinator's message channel:
    /// the slots and their allowed occupants.
    fn emit_sequence_registry(&mut self) {
        let _ = self.seq_registry_out.emit(&self.seq_registry);
    }

    /// Emit the full mission IR on the `wiring` channel — the live/historical
    /// topology the panel graph tile consumes. A no-op when no front-end set a
    /// manifest.
    fn emit_wiring_manifest(&mut self) {
        if let (Some(out), Some(manifest)) = (&mut self.wiring_out, &self.wiring_manifest) {
            let _ = out.emit(manifest);
        }
    }

    /// Emit the boot registry and manifest exactly once, at the head of the
    /// run, so a tap claimed after `build()` observes them ahead of any live
    /// edge activity. Idempotent: the `_emitted` latches gate re-entry.
    fn emit_boot(&mut self) {
        if !self.seq_registry_emitted {
            self.emit_sequence_registry();
            self.seq_registry_emitted = true;
        }
        if !self.wiring_emitted {
            self.emit_wiring_manifest();
            self.wiring_emitted = true;
        }
    }

    /// Drain the cycle's reload requests; on any request re-emit the registry
    /// and (unchanged) manifest so a consumer that missed boot resyncs off one
    /// message. The drain coalesces a burst into a single re-emission.
    fn service_reload(&mut self) -> Result<(), metor_fsw_ring::ReadError> {
        let mut reload = false;
        self.reload_in.drain(|ReloadSequences {}| reload = true)?;
        if reload {
            self.emit_sequence_registry();
            self.emit_wiring_manifest();
        }
        Ok(())
    }

    /// Take the single command writer; `None` after the first take.
    fn take_control(&mut self) -> Option<MsgOut<SequenceCommand>> {
        self.control_out.take()
    }
}

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
    /// can read while `run_for` holds `&mut self`. Purely observational.
    progress: Arc<AtomicU64>,
    /// The one broad registry over every registered buffer, untelemetered
    /// entries included.
    registry: Arc<Registry>,
    channels: CoordChannels,
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
        self.channels.take_control()
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
        self.channels.emit_boot();
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
            // A reload request re-emits the registry and manifest for consumers
            // that missed the boot message; the drain coalesces a burst of
            // requests into one emission per cycle.
            if self.channels.service_reload().is_err() {
                self.coord_health.error("reload_input_corrupt");
            }
            // Each slot drains its own `commands` fan-in at the head of `step`,
            // so a command dispatches the same cycle it lands — no
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
                // Simulated: no pacing, but still yield once so any spawned
                // async consumer gets to run on this cooperative runtime.
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
            let cancel = stellarator::util::CancelToken::new();
            let ctx = LaunchCtx {
                cancel: cancel.clone(),
                ready: ready.clone(),
                ready_count: ready_count.clone(),
                go: go.clone(),
                go_flag: go_flag.clone(),
            };
            let handle = pending.launcher.launch(ctx);
            tasks.push(AsyncTask {
                name: pending.name,
                handle: Some(handle),
                cancel,
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
        let result = self.status_out.write_with(&frame, |fw| {
            let _ = fw.list(
                &frame.stopped,
                offset_of!(CoordinatorStatus, stopped),
                |l| {
                    for sys in stopped.iter().take(MAX_STOPPED) {
                        let (name, len) = pack_name(&sys.name);
                        l.push(StoppedEntry {
                            reason: sys.reason.code(),
                            len,
                            _pad: [0; 6],
                            name,
                        });
                    }
                },
            );
            let _ = fw.list(
                &frame.workers,
                offset_of!(CoordinatorStatus, workers),
                |l| {
                    for w in workers.iter().take(MAX_WORKERS) {
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
                },
            );
        });
        if result.is_err() {
            self.coord_health.error("status_publish_failed");
        }
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

    /// Cooperative teardown: cancel waits, join tasks that run their shutdown
    /// hook, then report and force-cancel deadline offenders.
    async fn shutdown(&mut self, tasks: Vec<AsyncTask>) {
        for t in &tasks {
            t.cancel.cancel();
        }
        let deadline = Instant::now() + JOIN_TIMEOUT;
        while tasks
            .iter()
            .any(|task| !task.handle.as_ref().expect("task handle").0.is_complete())
            && Instant::now() < deadline
        {
            stellarator::yield_now().await;
        }
        for mut task in tasks {
            let handle = task.handle.take().expect("task handle");
            if !handle.0.is_complete() {
                self.coord_health.error("async_shutdown_timeout");
                self.coord_health.log(
                    Level::Error,
                    &format!("async system '{}' exceeded shutdown deadline", task.name),
                );
                let _ = handle.0.cancel();
            }
            let _ = handle.await;
        }
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
