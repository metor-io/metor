//! The host half of a worker process: spawning it, driving it in lockstep
//! with the cycle, and cleaning up after its death.
//!
//! Two lifecycles share one core. [`ProcSlot`] is the process twin of
//! [`DlSlot`](crate::dl): a fixed system driven for the whole run, restarted
//! on death within a budget. [`SeqWorker`] is the per-Load twin behind a
//! process slot's occupant (`docs/process-slots.md`): spawned by `Load`,
//! stepped while the occupant runs, and ended — kill, reap, reclaim — when
//! the occupant is stopped, reset, unloaded, or replaced. Both embed
//! [`WorkerHandle`], the spawn/poll/kill mechanics; what differs is policy
//! (restart vs. latch-and-report). [`describe_via_worker`] is the
//! resolve-time helper that obtains a system's descriptor bytes without ever
//! loading the artifact into this process.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use metor_fsw_ring::RingBuffer;
use metor_proto::types::Timestamp;

use crate::abi::FswStatus;
use crate::coordinator::{CyclicSlot, ProcInfo, SlotState, StopReason, WorkerRunState};

use super::ctl::{CtlHost, StepOutcome, WorkerState};
use super::session::SessionDir;
use super::worker::{RunMode, WORKER_ENV, WorkerManifest};

/// How long a spawned worker gets to report `Attached` (it must map rings and
/// dlopen the artifact) or a describe worker to exit. Generous: a healthy
/// worker takes milliseconds, and the common failure this bounds is a child
/// that ran the application's `main` because the guard is missing.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `init` waits for the worker's bind + `System::init`.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a shutdown request gets before the child is killed outright.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// The hint appended to every "worker never came up" diagnostic; the usual
/// cause is a host binary that re-executed into its own `main`.
const GUARD_HINT: &str = "if this binary embeds metor-fsw-2, its main must call \
     metor_fsw_2::proc::worker_entry() before anything else";

/// A describe-worker failure, surfaced through wiring resolve.
#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("cannot locate the worker executable: {0}")]
    WorkerExe(std::io::Error),
    #[error("describe worker I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("describe worker timed out after {timeout:?}; {GUARD_HINT}")]
    DescribeTimeout { timeout: Duration },
    #[error("describe worker exited with code {code}: {stderr}")]
    DescribeFailed { code: i32, stderr: String },
}

/// The worker executable: the caller's override, or this very binary
/// re-executed (the [`worker_entry`](super::worker_entry) guard routes it).
pub(crate) fn resolve_worker_exe(overridden: Option<&Path>) -> Result<PathBuf, ProcError> {
    match overridden {
        Some(exe) => Ok(exe.to_path_buf()),
        None => std::env::current_exe().map_err(ProcError::WorkerExe),
    }
}

/// Run a describe-mode worker over `artifact` and return the raw postcard
/// [`SystemDescriptorMsg`](crate::abi::SystemDescriptorMsg) bytes it wrote —
/// the host-side twin of `fsw_describe`, with the dlopen quarantined in a
/// short-lived child. Decode the bytes with
/// [`SystemDescriptorMsg::into_descriptor`](crate::abi::SystemDescriptorMsg::into_descriptor)
/// and register via
/// [`add_proc_cyclic`](crate::CoordinatorBuilder::add_proc_cyclic);
/// [`resolve`](crate::wiring::resolve) does exactly this. The
/// child's stderr is captured into the failure diagnostic. `worker_exe`
/// `None` re-executes this binary (whose `main` must call [`worker_entry`]).
///
/// [`worker_entry`]: super::worker_entry
pub fn describe_via_worker(
    worker_exe: Option<&Path>,
    artifact: &Path,
) -> Result<Vec<u8>, ProcError> {
    let exe = resolve_worker_exe(worker_exe)?;
    let dir = SessionDir::create(None)?;
    let manifest_path = dir.path().join("describe.manifest");
    let out = dir.path().join("describe.out");
    let manifest = WorkerManifest::Describe {
        artifact: artifact.to_path_buf(),
        out: out.clone(),
    };
    std::fs::write(
        &manifest_path,
        postcard::to_allocvec(&manifest).expect("manifest encodes (postcard)"),
    )?;
    let mut child = Command::new(&exe)
        .env(WORKER_ENV, &manifest_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProcError::DescribeTimeout {
                timeout: SPAWN_TIMEOUT,
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        return Err(ProcError::DescribeFailed {
            code: status.code().unwrap_or(-1),
            stderr: stderr.trim().to_string(),
        });
    }
    Ok(std::fs::read(&out)?)
}

/// Everything `build()` gathers to spawn one run worker.
pub(crate) struct SpawnSpec {
    /// The instance name (the worker's health/status identity).
    pub instance: String,
    /// The pack entry the worker instantiates.
    pub system: String,
    /// The pack cdylib the worker loads.
    pub artifact: PathBuf,
    /// Canonical postcard `Params` bytes.
    pub params: Vec<u8>,
    /// Session-dir file for this worker's control block.
    pub ctl_path: PathBuf,
    /// Session-dir file for this worker's manifest.
    pub manifest_path: PathBuf,
    /// Ring files in descriptor order — the positional bind contract.
    pub input_paths: Vec<PathBuf>,
    pub output_paths: Vec<PathBuf>,
    /// Host handles of every ring the worker will attach (its outputs plus
    /// its producers' outputs), kept for owner reclamation on death.
    pub rings: Vec<RingBuffer>,
    /// Override of the worker executable; `None` re-executes this binary.
    pub worker_exe: Option<PathBuf>,
    /// Per-step ack deadline ([`CoordinatorConfig::proc_step_timeout`](crate::CoordinatorConfig)).
    pub step_timeout: Duration,
    /// Respawn budget ([`CoordinatorConfig::proc_max_restarts`](crate::CoordinatorConfig)).
    pub max_restarts: u32,
    /// Delay before each respawn ([`CoordinatorConfig::proc_restart_backoff`](crate::CoordinatorConfig)).
    pub restart_backoff: Duration,
    /// The slot identity, already leaked to `'static` by the caller.
    pub name: &'static str,
}

// ---------------------------------------------------------------------------
// WorkerHandle, the shared spawn/poll/kill core
// ---------------------------------------------------------------------------

/// The process-management core [`ProcSlot`] and [`SeqWorker`] share: the ctl
/// block, the child, the host handles of every ring the worker attaches (the
/// reclaim set), and the paths a spawn needs. It owns the mechanics — spawn
/// over a fresh control block, one non-blocking poll toward a wanted state,
/// end-for-certain — while the policy (when to spawn, what a failure means)
/// stays with the embedding type.
struct WorkerHandle {
    ctl: CtlHost,
    /// The live child; `None` before the first spawn and between reap and respawn.
    child: Option<Child>,
    /// Host handles of the worker-attached rings, for reclamation.
    rings: Vec<RingBuffer>,
    /// The resolved worker executable.
    exe: PathBuf,
    ctl_path: PathBuf,
    /// The on-disk manifest every spawn of this worker runs from.
    manifest_path: PathBuf,
}

/// How one non-blocking [`WorkerHandle::poll_state`] resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandlePoll {
    /// Not there yet; poll again next cycle.
    Pending,
    /// The worker reported the wanted state.
    Ready,
    /// The worker reported failure, exited, or blew the deadline.
    Failed,
}

impl WorkerHandle {
    /// A handle over a fresh control block at `ctl_path`, no child yet.
    fn new(
        exe: PathBuf,
        ctl_path: PathBuf,
        manifest_path: PathBuf,
        rings: Vec<RingBuffer>,
    ) -> Result<Self, super::ctl::CtlError> {
        let ctl = CtlHost::create(&ctl_path)?;
        Ok(Self {
            ctl,
            child: None,
            rings,
            exe,
            ctl_path,
            manifest_path,
        })
    }

    /// Spawn a worker from the persisted manifest over a **fresh** control
    /// block (recreating the file resets the lifecycle to `Booting` and the
    /// sequence words to zero, matching the fresh worker's).
    fn spawn_child(&mut self) -> std::io::Result<()> {
        self.ctl = CtlHost::create(&self.ctl_path).map_err(|e| match e {
            super::ctl::CtlError::Io(io) => io,
            other => std::io::Error::other(other.to_string()),
        })?;
        let child = Command::new(&self.exe)
            .env(WORKER_ENV, &self.manifest_path)
            .stdin(Stdio::null())
            .spawn()?;
        self.child = Some(child);
        Ok(())
    }

    /// The live child's pid, or `0` between workers.
    fn pid(&self) -> u32 {
        self.child.as_ref().map(|c| c.id()).unwrap_or(0)
    }

    /// End the child for certain and free everything it claimed: kill (a
    /// no-op if already exited), reap, then reclaim its ring roles.
    fn kill_reap_reclaim(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        let _ = child.wait();
        let pid = child.id() as u64;
        for ring in &self.rings {
            // SAFETY: the child was reaped just above, so none of its stores
            // are in flight, and reclaiming immediately after the reap keeps
            // the pid-reuse window nil.
            unsafe { ring.reclaim_owner(pid) };
        }
    }

    /// Whether the child is certainly gone: reaped by `try_wait`, or absent.
    /// A `try_wait` error reads as alive, so an indeterminate child is
    /// telemetered as late rather than escalated to a kill.
    fn child_dead(&mut self) -> bool {
        match self.child.as_mut().map(|c| c.try_wait()) {
            Some(Ok(Some(_))) | None => true,
            Some(Ok(None)) | Some(Err(_)) => false,
        }
    }

    /// One non-blocking poll toward `want`: has the worker failed, died, or
    /// blown its `deadline`; and if none of those, has it reached `want`?
    fn poll_state(&mut self, want: WorkerState, deadline: Instant) -> HandlePoll {
        match self.ctl.state() {
            Some(WorkerState::Failed) | None => return HandlePoll::Failed,
            Some(state) if state == want => return HandlePoll::Ready,
            _ => {}
        }
        let exited = matches!(self.child.as_mut().map(|c| c.try_wait()), Some(Ok(Some(_))));
        if exited || Instant::now() >= deadline {
            return HandlePoll::Failed;
        }
        HandlePoll::Pending
    }

    /// The graceful end for mission teardown: request shutdown, give the
    /// worker the grace window to report and exit, then end it for certain
    /// either way. Blocking is acceptable here, unlike the cycle loop.
    fn shutdown_graceful(&mut self) {
        if self.child.is_none() {
            return; // between workers, or ended: already reaped and reclaimed
        }
        self.ctl.request(WorkerState::ShutdownReq);
        let _ = self.ctl.wait_state(WorkerState::Done, SHUTDOWN_GRACE);
        // Reap within the grace window; then end it for certain either way.
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            if matches!(self.child.as_mut().map(|c| c.try_wait()), Some(Ok(Some(_)))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.kill_reap_reclaim();
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // A clean shutdown already reaped (kill/wait/reclaim are idempotent);
        // this covers an embedding slot dropped mid-run.
        self.kill_reap_reclaim();
    }
}

// ---------------------------------------------------------------------------
// ProcSlot, the whole-run worker behind a process system
// ---------------------------------------------------------------------------

/// Where a process slot is in its worker's lifecycle, beyond the coarse
/// [`SlotState`]. `Running` is the steady state; the middle three are the
/// non-blocking restart pipeline (each polled once per cycle, so a respawn
/// never stalls the loop the way the initial blocking spawn at `build()`
/// may); `Terminal` is a stop past the restart budget.
enum Phase {
    /// A live worker being stepped.
    Running,
    /// Dead, reaped, reclaimed; waiting out the backoff before respawning.
    Backoff { until: Instant },
    /// Respawned; waiting for the fresh worker to report `Attached`.
    Attaching { deadline: Instant },
    /// Init requested; waiting for `Ready`, then the slot resumes.
    Initing { deadline: Instant },
    /// Out of restart budget; `slot_state` holds the final reason.
    Terminal,
}

/// A running worker process behind the `CyclicSlot` interface, the process
/// twin of [`DlSlot`](crate::dl). `init` runs the worker's bind+init behind
/// the coordinator's init barrier, `step` rings the doorbell and waits out
/// the ack (bounded by the step deadline), and `shutdown`/`Drop` end the
/// child and reclaim whatever it left claimed in the shared rings.
///
/// # Restart
///
/// A worker that dies — or whose system panics, which in a worker is safely
/// quarantined — is respawned up to `max_restarts` times over the slot's
/// life: kill/reap/reclaim, wait out the backoff, respawn from the same
/// on-disk manifest (same ring files, same params), and re-init. While
/// restarting, `slot_state` reports the stop (so the outage is telemetered)
/// and flips back to `Running` on recovery; the fresh worker's views start
/// at the rings' current positions, so records committed during the outage
/// are skipped, not replayed. Restarts are counted, drained into coordinator
/// health, and carried in the status frame's worker list.
pub(crate) struct ProcSlot {
    name: &'static str,
    /// The worker process behind this slot, respawned from the persisted
    /// manifest across restarts.
    handle: WorkerHandle,
    step_timeout: Duration,
    slot_state: SlotState,
    /// Steps whose ack deadline lapsed with the child still alive, since the
    /// coordinator last drained them into its health.
    timeouts: u64,
    // --- restart machinery -------------------------------------------------
    phase: Phase,
    max_restarts: u32,
    backoff: Duration,
    /// Restarts begun over the slot's life (each spawn attempt costs one).
    restarts: u32,
    /// Restarts begun since the coordinator last drained them.
    restart_events: u64,
}

impl ProcSlot {
    /// Create the control block, write the manifest, spawn the worker, and
    /// wait for `Attached`. Any failure kills the child and reports why —
    /// `build()` maps the message into a `WireError`. This first spawn is the
    /// only blocking one; respawns are polled from `step`.
    pub(crate) fn spawn(spec: SpawnSpec) -> Result<Self, String> {
        let exe = resolve_worker_exe(spec.worker_exe.as_deref()).map_err(|e| e.to_string())?;
        let manifest = WorkerManifest::Run {
            abi_version: crate::abi::FSW_ABI_VERSION,
            mode: RunMode::Cyclic,
            instance: spec.instance,
            system: spec.system,
            artifact: spec.artifact,
            params: spec.params,
            ctl: spec.ctl_path.clone(),
            inputs: spec.input_paths,
            outputs: spec.output_paths,
        };
        std::fs::write(
            &spec.manifest_path,
            postcard::to_allocvec(&manifest).expect("manifest encodes (postcard)"),
        )
        .map_err(|e| format!("manifest: {e}"))?;
        let handle = WorkerHandle::new(exe, spec.ctl_path, spec.manifest_path, spec.rings)
            .map_err(|e| format!("control block: {e}"))?;
        let mut slot = ProcSlot {
            name: spec.name,
            handle,
            step_timeout: spec.step_timeout,
            slot_state: SlotState::Running,
            timeouts: 0,
            phase: Phase::Running,
            max_restarts: spec.max_restarts,
            backoff: spec.restart_backoff,
            restarts: 0,
            restart_events: 0,
        };
        slot.handle
            .spawn_child()
            .map_err(|e| format!("spawn `{}`: {e}", slot.handle.exe.display()))?;
        if let Err(e) = slot.handle.ctl.wait_state(WorkerState::Attached, SPAWN_TIMEOUT) {
            slot.handle.kill_reap_reclaim();
            return Err(format!("worker never attached ({e}); {GUARD_HINT}"));
        }
        Ok(slot)
    }

    /// Schedule the next restart attempt, or go terminal past the budget.
    /// Every attempt costs one unit of budget, so a worker that dies during
    /// its own restart pipeline cannot loop for free.
    fn schedule_restart(&mut self) {
        self.phase = if self.restarts < self.max_restarts {
            self.restarts += 1;
            self.restart_events += 1;
            Phase::Backoff {
                until: Instant::now() + self.backoff,
            }
        } else {
            Phase::Terminal
        };
    }

    /// The running worker failed (`reason` says how): end it, report the
    /// stop, and enter the restart pipeline (budget permitting).
    fn fail_worker(&mut self, reason: StopReason) {
        self.handle.kill_reap_reclaim();
        self.slot_state = SlotState::Stopped { reason };
        self.schedule_restart();
    }

    /// A restart attempt failed before reaching `Ready`: end the half-born
    /// worker (it may already hold ring claims) and try again. The reported
    /// `slot_state` keeps the original stop reason.
    fn attempt_failed(&mut self) {
        self.handle.kill_reap_reclaim();
        self.schedule_restart();
    }

    /// One restart-phase poll, folding a [`HandlePoll::Failed`] into the next
    /// restart attempt.
    fn poll_worker(&mut self, want: WorkerState, deadline: Instant) -> bool {
        match self.handle.poll_state(want, deadline) {
            HandlePoll::Ready => true,
            HandlePoll::Pending => false,
            HandlePoll::Failed => {
                self.attempt_failed();
                false
            }
        }
    }
}

impl CyclicSlot for ProcSlot {
    fn init(&mut self) {
        self.handle.ctl.request(WorkerState::InitReq);
        if self
            .handle
            .ctl
            .wait_state(WorkerState::Ready, INIT_TIMEOUT)
            .is_err()
        {
            self.fail_worker(StopReason::ProcessDied);
        }
    }

    fn step(&mut self, now: Timestamp) {
        match self.phase {
            Phase::Running => match self.handle.ctl.step(now, self.step_timeout) {
                // A stray `Done` folds to keep-running, as in `DlSlot::step`.
                StepOutcome::Acked(FswStatus::Running | FswStatus::Done) => {}
                // In a worker a panic is fully quarantined (the foreign state
                // was already destroyed on its side, freeing its ring roles),
                // so unlike the in-process dl path it is restartable.
                StepOutcome::Acked(FswStatus::Panicked) => {
                    self.fail_worker(StopReason::Panicked);
                }
                StepOutcome::TimedOut => {
                    if self.handle.child_dead() {
                        // The worker is gone; the abandoned sequence never resolves.
                        self.fail_worker(StopReason::ProcessDied);
                    } else {
                        // Alive but late: telemetered, and the sequence protocol
                        // self-heals (the worker serves only the newest doorbell).
                        self.timeouts += 1;
                    }
                }
            },
            Phase::Backoff { until } => {
                if Instant::now() < until {
                    return;
                }
                match self.handle.spawn_child() {
                    Ok(()) => {
                        self.phase = Phase::Attaching {
                            deadline: Instant::now() + SPAWN_TIMEOUT,
                        };
                    }
                    Err(_) => self.attempt_failed(),
                }
            }
            Phase::Attaching { deadline } => {
                if self.poll_worker(WorkerState::Attached, deadline) {
                    self.handle.ctl.request(WorkerState::InitReq);
                    self.phase = Phase::Initing {
                        deadline: Instant::now() + INIT_TIMEOUT,
                    };
                }
            }
            Phase::Initing { deadline } => {
                if self.poll_worker(WorkerState::Ready, deadline) {
                    // Recovered: the outage was telemetered through the
                    // stopped set; clear it and resume stepping next cycle.
                    self.slot_state = SlotState::Running;
                    self.phase = Phase::Running;
                }
            }
            Phase::Terminal => {}
        }
    }

    fn shutdown(&mut self) {
        self.handle.shutdown_graceful();
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn state(&self) -> &SlotState {
        &self.slot_state
    }

    fn drain_timeouts(&mut self) -> u64 {
        std::mem::take(&mut self.timeouts)
    }

    fn drain_restarts(&mut self) -> u64 {
        std::mem::take(&mut self.restart_events)
    }

    fn proc_info(&self) -> Option<ProcInfo> {
        let state = match self.phase {
            Phase::Running => WorkerRunState::Running,
            Phase::Terminal => WorkerRunState::Stopped,
            Phase::Backoff { .. } | Phase::Attaching { .. } | Phase::Initing { .. } => {
                WorkerRunState::Restarting
            }
        };
        Some(ProcInfo {
            pid: self.handle.pid(),
            restarts: self.restarts,
            state,
        })
    }
}

// ---------------------------------------------------------------------------
// SeqWorker, the per-Load worker behind a process slot occupant
// ---------------------------------------------------------------------------

/// How far a [`SeqWorker`]'s load pipeline has advanced. The waiting phases
/// carry their deadline; the terminal phases are latched, so a poll past the
/// end re-serves the answer instead of re-driving the ctl block.
enum LoadPhase {
    /// Spawned; waiting for `Attached` (ring maps, dlopen, `fsw_create`).
    Attaching { deadline: Instant },
    /// Init requested; waiting for `Ready` (`fsw_bind_init` claimed the ring
    /// roles and built the occupant future).
    Initing { deadline: Instant },
    /// Bound and steppable.
    Ready,
    /// The pipeline died at `stage`; the worker is already ended.
    Failed { stage: &'static str },
}

/// How one [`SeqWorker::poll_load`] resolved. `Failed` names the pipeline
/// stage that died, so the runner's `Failed` event can tell the operator
/// where.
pub(crate) enum LoadPoll {
    Pending,
    Ready,
    Failed { stage: &'static str },
}

/// The worker behind one process-slot occupant Load (`docs/process-slots.md`):
/// where [`ProcSlot`] drives a fixed system for the whole run, a `SeqWorker`
/// lives exactly one Load cycle — spawned when the runner loads an occupant,
/// stepped while it runs, and ended (kill + reap + reclaim, the process twin
/// of the slot's hard-drop) on `Stop`/`Reset`/`Unload` or the next `Load`.
///
/// It shares [`ProcSlot`]'s spawn/poll/kill core through [`WorkerHandle`];
/// what differs is the policy. The load pipeline is `ProcSlot`'s restart
/// machine minus the backoff, and a failure anywhere in it does not respawn:
/// it is latched (stage and all) for the runner to fold into the slot's
/// terminal states, because re-running a sequence re-issues its side effects
/// — an operator decision, not a supervisor default.
pub(crate) struct SeqWorker {
    handle: WorkerHandle,
    /// Per-step ack deadline
    /// ([`CoordinatorConfig::proc_step_timeout`](crate::CoordinatorConfig)).
    step_timeout: Duration,
    phase: LoadPhase,
}

impl SeqWorker {
    /// Spawn a worker for one occupant from its persisted manifest, entering
    /// the polled pipeline. Non-blocking, and nothing is killed here: tearing
    /// down any previous worker is the caller's job, *before* this spawn, so
    /// the fresh worker's `fsw_bind_init` claims ring roles only after the
    /// old ones were reclaimed — the reader-budget invariant.
    pub(crate) fn spawn(
        exe: &Path,
        ctl_path: &Path,
        manifest_path: &Path,
        rings: Vec<RingBuffer>,
        step_timeout: Duration,
    ) -> Result<Self, String> {
        let mut handle = WorkerHandle::new(
            exe.to_path_buf(),
            ctl_path.to_path_buf(),
            manifest_path.to_path_buf(),
            rings,
        )
        .map_err(|e| format!("control block: {e}"))?;
        handle
            .spawn_child()
            .map_err(|e| format!("spawn `{}`: {e}", exe.display()))?;
        Ok(SeqWorker {
            handle,
            step_timeout,
            phase: LoadPhase::Attaching {
                deadline: Instant::now() + SPAWN_TIMEOUT,
            },
        })
    }

    /// Advance the pipeline at most one phase: one state observation per
    /// call, so a Load in flight costs the cycle loop a poll, never a wait.
    /// `Ready` and `Failed` are latched and re-served.
    pub(crate) fn poll_load(&mut self) -> LoadPoll {
        match self.phase {
            LoadPhase::Attaching { deadline } => {
                match self.handle.poll_state(WorkerState::Attached, deadline) {
                    HandlePoll::Ready => {
                        self.handle.ctl.request(WorkerState::InitReq);
                        self.phase = LoadPhase::Initing {
                            deadline: Instant::now() + INIT_TIMEOUT,
                        };
                        LoadPoll::Pending
                    }
                    HandlePoll::Pending => LoadPoll::Pending,
                    HandlePoll::Failed => self.fail_stage("attach"),
                }
            }
            LoadPhase::Initing { deadline } => {
                match self.handle.poll_state(WorkerState::Ready, deadline) {
                    HandlePoll::Ready => {
                        self.phase = LoadPhase::Ready;
                        LoadPoll::Ready
                    }
                    HandlePoll::Pending => LoadPoll::Pending,
                    HandlePoll::Failed => self.fail_stage("init"),
                }
            }
            LoadPhase::Ready => LoadPoll::Ready,
            LoadPhase::Failed { stage } => LoadPoll::Failed { stage },
        }
    }

    /// Drive the pipeline to completion, blocking on the ctl word — the
    /// init-barrier variant for a slot's initial occupant, mirroring the
    /// deliberately blocking build-time [`ProcSlot::spawn`]: init is not
    /// cycle time. `Err` carries the failed stage; the worker is already
    /// ended.
    pub(crate) fn wait_ready(&mut self) -> Result<(), &'static str> {
        loop {
            match self.phase {
                LoadPhase::Attaching { deadline } => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if self
                        .handle
                        .ctl
                        .wait_state(WorkerState::Attached, left)
                        .is_err()
                    {
                        self.fail_stage("attach");
                    } else {
                        self.handle.ctl.request(WorkerState::InitReq);
                        self.phase = LoadPhase::Initing {
                            deadline: Instant::now() + INIT_TIMEOUT,
                        };
                    }
                }
                LoadPhase::Initing { deadline } => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if self
                        .handle
                        .ctl
                        .wait_state(WorkerState::Ready, left)
                        .is_err()
                    {
                        self.fail_stage("init");
                    } else {
                        self.phase = LoadPhase::Ready;
                    }
                }
                LoadPhase::Ready => return Ok(()),
                LoadPhase::Failed { stage } => return Err(stage),
            }
        }
    }

    /// The pipeline died at `stage` (failure report, early exit, deadline):
    /// end the half-born worker — it may already hold ring claims — and
    /// latch the failure for the runner's fold.
    fn fail_stage(&mut self, stage: &'static str) -> LoadPoll {
        self.end();
        self.phase = LoadPhase::Failed { stage };
        LoadPoll::Failed { stage }
    }

    /// Ring one doorbell and wait out the ack, bounded by the step deadline.
    pub(crate) fn step(&mut self, now: Timestamp) -> StepOutcome {
        self.handle.ctl.step(now, self.step_timeout)
    }

    /// The live worker's pid, or `0` after `end`.
    pub(crate) fn pid(&self) -> u32 {
        self.handle.pid()
    }

    /// Whether the worker process is certainly gone — the step-timeout fork:
    /// dead means the occupant will never ack, alive means merely late.
    pub(crate) fn child_dead(&mut self) -> bool {
        self.handle.child_dead()
    }

    /// End the worker for certain and free everything it claimed. Idempotent,
    /// and also the drop behavior (through [`WorkerHandle`]). Kill rather
    /// than a graceful request: the in-process `Stop` is a hard drop with no
    /// async cleanup, `run_seq_shutdown` is a documented no-op, and
    /// `reclaim_owner` frees the only thing a kill leaves behind.
    pub(crate) fn end(&mut self) {
        self.handle.kill_reap_reclaim();
    }

    /// Mission teardown: the graceful exit `Stop` deliberately skips —
    /// shutdown request, grace window, then kill — matching [`ProcSlot`]'s,
    /// since blocking is acceptable there.
    pub(crate) fn shutdown(&mut self) {
        self.handle.shutdown_graceful();
    }
}

#[cfg(test)]
mod tests {
    //! [`SeqWorker`] pipeline tests over a fake ctl worker: the ctl protocol
    //! is address-space agnostic, so the test thread plays the occupant
    //! worker over the ctl file while the actually spawned child is an inert
    //! stand-in (`/bin/sh`, which exits immediately on its null stdin). The
    //! polls under test observe the ctl word before the child, so the
    //! stand-in's exit matters only where a test wants it to.

    use super::*;
    use crate::proc::ctl::CtlWorker;
    use crate::proc::worker::fail_code;

    fn spawn_seq(dir: &tempfile::TempDir) -> SeqWorker {
        let manifest = dir.path().join("occ.manifest");
        std::fs::write(&manifest, b"unread by the stand-in child").unwrap();
        SeqWorker::spawn(
            Path::new("/bin/sh"),
            &dir.path().join("slot.ctl"),
            &manifest,
            Vec::new(),
            Duration::from_millis(50),
        )
        .expect("stand-in spawns")
    }

    /// The happy path is exactly two observations: `Attached` advances the
    /// pipeline (requesting init), `Ready` completes it; both terminals are
    /// latched.
    #[test]
    fn pipeline_reaches_ready_in_two_polls() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = spawn_seq(&dir);
        let ctl = CtlWorker::attach(&dir.path().join("slot.ctl")).unwrap();

        ctl.report(WorkerState::Attached);
        assert!(matches!(w.poll_load(), LoadPoll::Pending));
        // The poll requested init, so the fake's wait returns immediately.
        assert!(ctl.wait_init());
        ctl.report(WorkerState::Ready);
        assert!(matches!(w.poll_load(), LoadPoll::Ready));
        assert!(matches!(w.poll_load(), LoadPoll::Ready), "Ready is latched");

        assert_ne!(w.pid(), 0);
        w.end();
        assert_eq!(w.pid(), 0);
        w.end(); // idempotent
    }

    /// `wait_ready`, the blocking init-barrier variant, drives the same
    /// pipeline to the same latched `Ready`.
    #[test]
    fn wait_ready_blocks_out_the_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = spawn_seq(&dir);
        let ctl = CtlWorker::attach(&dir.path().join("slot.ctl")).unwrap();

        let fake = std::thread::spawn(move || {
            ctl.report(WorkerState::Attached);
            assert!(ctl.wait_init());
            ctl.report(WorkerState::Ready);
        });
        assert_eq!(w.wait_ready(), Ok(()));
        assert!(matches!(w.poll_load(), LoadPoll::Ready));
        fake.join().unwrap();
        w.end();
    }

    /// A worker-side failure report during attach ends the half-born worker
    /// and latches the stage.
    #[test]
    fn attach_failure_folds_and_latches() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = spawn_seq(&dir);
        let ctl = CtlWorker::attach(&dir.path().join("slot.ctl")).unwrap();

        ctl.fail(fail_code::ARTIFACT);
        assert!(matches!(w.poll_load(), LoadPoll::Failed { stage: "attach" }));
        assert_eq!(w.pid(), 0, "the half-born worker was ended");
        assert!(
            matches!(w.poll_load(), LoadPoll::Failed { stage: "attach" }),
            "Failed is latched"
        );
        w.end(); // idempotent past the fold
    }

    /// A failure after attach lands on the init stage, so the runner's event
    /// names where the pipeline died.
    #[test]
    fn init_failure_names_its_stage() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = spawn_seq(&dir);
        let ctl = CtlWorker::attach(&dir.path().join("slot.ctl")).unwrap();

        ctl.report(WorkerState::Attached);
        assert!(matches!(w.poll_load(), LoadPoll::Pending));
        ctl.fail(fail_code::CREATE);
        assert!(matches!(w.poll_load(), LoadPoll::Failed { stage: "init" }));
    }

    /// A stand-in child that exits without any worker report folds to
    /// `Failed` (the early-exit arm of the poll).
    #[test]
    fn early_exit_folds_failed() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = spawn_seq(&dir);
        let give_up = Instant::now() + Duration::from_secs(10);
        loop {
            match w.poll_load() {
                LoadPoll::Failed { stage } => {
                    assert_eq!(stage, "attach");
                    break;
                }
                LoadPoll::Pending => {
                    assert!(Instant::now() < give_up, "exit never observed");
                    std::thread::sleep(Duration::from_millis(2));
                }
                LoadPoll::Ready => unreachable!("nothing ever reported"),
            }
        }
        w.end();
    }

    /// A silent-but-alive worker past its window folds on the deadline
    /// instead of being waited on forever. The child is taken so only the
    /// deadline can trigger the fold.
    #[test]
    fn deadline_lapse_folds_failed() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = spawn_seq(&dir);
        w.handle.kill_reap_reclaim();
        w.phase = LoadPhase::Attaching {
            deadline: Instant::now(),
        };
        assert!(matches!(w.poll_load(), LoadPoll::Failed { stage: "attach" }));
        w.end();
    }
}
