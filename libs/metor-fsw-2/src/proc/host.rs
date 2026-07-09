//! The host half of a process system: spawning the worker, driving it as a
//! cyclic slot, and cleaning up after its death.
//!
//! [`ProcSlot`] is the process twin of [`DlSlot`](crate::dl): the coordinator
//! drives it through the same `CyclicSlot` interface, but `init`, `step`, and
//! `shutdown` cross the [`ctl`](super::ctl) protocol instead of the C ABI.
//! [`describe_via_worker`] is the resolve-time helper that obtains a system's
//! descriptor bytes without ever loading the artifact into this process.

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
fn resolve_worker_exe(overridden: Option<&Path>) -> Result<PathBuf, ProcError> {
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
/// [`add_proc_cyclic`](crate::CoordinatorBuilder::add_proc_cyclic); the KDL
/// [`resolve`](crate::wiring::resolve) front-end does exactly this. The
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
    /// The system cdylib the worker loads.
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
    ctl: CtlHost,
    /// The live child; `None` between reap and respawn.
    child: Option<Child>,
    /// Host handles of the worker-attached rings, for reclamation.
    rings: Vec<RingBuffer>,
    step_timeout: Duration,
    slot_state: SlotState,
    /// Steps whose ack deadline lapsed with the child still alive, since the
    /// coordinator last drained them into its health.
    timeouts: u64,
    // --- restart machinery -------------------------------------------------
    phase: Phase,
    /// The resolved worker executable, respawned from the persisted manifest.
    exe: PathBuf,
    ctl_path: PathBuf,
    manifest_path: PathBuf,
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
        let ctl = CtlHost::create(&spec.ctl_path).map_err(|e| format!("control block: {e}"))?;
        let mut slot = ProcSlot {
            name: spec.name,
            ctl,
            child: None,
            rings: spec.rings,
            step_timeout: spec.step_timeout,
            slot_state: SlotState::Running,
            timeouts: 0,
            phase: Phase::Running,
            exe,
            ctl_path: spec.ctl_path,
            manifest_path: spec.manifest_path,
            max_restarts: spec.max_restarts,
            backoff: spec.restart_backoff,
            restarts: 0,
            restart_events: 0,
        };
        slot.spawn_child().map_err(|e| format!("spawn `{}`: {e}", slot.exe.display()))?;
        if let Err(e) = slot.ctl.wait_state(WorkerState::Attached, SPAWN_TIMEOUT) {
            slot.kill_reap_reclaim();
            return Err(format!("worker never attached ({e}); {GUARD_HINT}"));
        }
        Ok(slot)
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
        self.kill_reap_reclaim();
        self.slot_state = SlotState::Stopped { reason };
        self.schedule_restart();
    }

    /// A restart attempt failed before reaching `Ready`: end the half-born
    /// worker (it may already hold ring claims) and try again. The reported
    /// `slot_state` keeps the original stop reason.
    fn attempt_failed(&mut self) {
        self.kill_reap_reclaim();
        self.schedule_restart();
    }

    /// One non-blocking poll of a restart phase: has the worker failed, died,
    /// or blown its `deadline`; and if none of those, has it reached `want`?
    fn poll_worker(&mut self, want: WorkerState, deadline: Instant) -> bool {
        match self.ctl.state() {
            Some(WorkerState::Failed) | None => {
                self.attempt_failed();
                return false;
            }
            Some(state) if state == want => return true,
            _ => {}
        }
        let exited = matches!(self.child.as_mut().map(|c| c.try_wait()), Some(Ok(Some(_))));
        if exited || Instant::now() >= deadline {
            self.attempt_failed();
        }
        false
    }
}

impl CyclicSlot for ProcSlot {
    fn init(&mut self) {
        self.ctl.request(WorkerState::InitReq);
        if self.ctl.wait_state(WorkerState::Ready, INIT_TIMEOUT).is_err() {
            self.fail_worker(StopReason::ProcessDied);
        }
    }

    fn step(&mut self, now: Timestamp) {
        match self.phase {
            Phase::Running => match self.ctl.step(now, self.step_timeout) {
                // A stray `Done` folds to keep-running, as in `DlSlot::step`.
                StepOutcome::Acked(FswStatus::Running | FswStatus::Done) => {}
                // In a worker a panic is fully quarantined (the foreign state
                // was already destroyed on its side, freeing its ring roles),
                // so unlike the in-process dl path it is restartable.
                StepOutcome::Acked(FswStatus::Panicked) => {
                    self.fail_worker(StopReason::Panicked);
                }
                StepOutcome::TimedOut => match self.child.as_mut().map(|c| c.try_wait()) {
                    // The worker is gone; the abandoned sequence never resolves.
                    Some(Ok(Some(_))) | None => self.fail_worker(StopReason::ProcessDied),
                    // Alive but late: telemetered, and the sequence protocol
                    // self-heals (the worker serves only the newest doorbell).
                    Some(Ok(None)) | Some(Err(_)) => self.timeouts += 1,
                },
            },
            Phase::Backoff { until } => {
                if Instant::now() < until {
                    return;
                }
                match self.spawn_child() {
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
                    self.ctl.request(WorkerState::InitReq);
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
        if self.child.is_none() {
            return; // between workers, or terminal: already reaped and reclaimed
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
            pid: self.pid(),
            restarts: self.restarts,
            state,
        })
    }
}

impl Drop for ProcSlot {
    fn drop(&mut self) {
        // A clean shutdown already reaped (kill/wait/reclaim are idempotent);
        // this covers a coordinator dropped mid-run.
        self.kill_reap_reclaim();
    }
}
