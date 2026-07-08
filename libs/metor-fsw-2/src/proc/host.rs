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
use crate::coordinator::{CyclicSlot, SlotState, StopReason};

use super::ctl::{CtlHost, StepOutcome, WorkerState};
use super::session::SessionDir;
use super::worker::{WORKER_ENV, WorkerManifest};

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
    /// The slot identity, already leaked to `'static` by the caller.
    pub name: &'static str,
}

/// A running worker process behind the `CyclicSlot` interface, the process
/// twin of [`DlSlot`](crate::dl). `init` runs the worker's bind+init behind
/// the coordinator's init barrier, `step` rings the doorbell and waits out
/// the ack (bounded by the step deadline), and `shutdown`/`Drop` end the
/// child and reclaim whatever it left claimed in the shared rings.
pub(crate) struct ProcSlot {
    name: &'static str,
    ctl: CtlHost,
    child: Child,
    /// Host handles of the worker-attached rings, for reclamation.
    rings: Vec<RingBuffer>,
    step_timeout: Duration,
    slot_state: SlotState,
    /// Steps whose ack deadline lapsed with the child still alive, since the
    /// coordinator last drained them into its health.
    timeouts: u64,
}

impl ProcSlot {
    /// Create the control block, write the manifest, spawn the worker, and
    /// wait for `Attached`. Any failure kills the child and reports why —
    /// `build()` maps the message into a `WireError`.
    pub(crate) fn spawn(spec: SpawnSpec) -> Result<Self, String> {
        let exe = resolve_worker_exe(spec.worker_exe.as_deref()).map_err(|e| e.to_string())?;
        let ctl = CtlHost::create(&spec.ctl_path).map_err(|e| format!("control block: {e}"))?;
        let manifest = WorkerManifest::Run {
            abi_version: crate::abi::FSW_ABI_VERSION,
            instance: spec.instance,
            artifact: spec.artifact,
            params: spec.params,
            ctl: spec.ctl_path,
            inputs: spec.input_paths,
            outputs: spec.output_paths,
        };
        std::fs::write(
            &spec.manifest_path,
            postcard::to_allocvec(&manifest).expect("manifest encodes (postcard)"),
        )
        .map_err(|e| format!("manifest: {e}"))?;
        let child = Command::new(&exe)
            .env(WORKER_ENV, &spec.manifest_path)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn `{}`: {e}", exe.display()))?;
        let mut slot = ProcSlot {
            name: spec.name,
            ctl,
            child,
            rings: spec.rings,
            step_timeout: spec.step_timeout,
            slot_state: SlotState::Running,
            timeouts: 0,
        };
        if let Err(e) = slot.ctl.wait_state(WorkerState::Attached, SPAWN_TIMEOUT) {
            slot.kill_reap_reclaim();
            return Err(format!("worker never attached ({e}); {GUARD_HINT}"));
        }
        Ok(slot)
    }

    /// End the child for certain and free everything it claimed: kill (a
    /// no-op if already exited), reap, then reclaim its ring roles.
    fn kill_reap_reclaim(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let pid = self.child.id() as u64;
        for ring in &self.rings {
            // SAFETY: the child was reaped just above, so none of its stores
            // are in flight, and reclaiming immediately after the reap keeps
            // the pid-reuse window nil.
            unsafe { ring.reclaim_owner(pid) };
        }
    }

    /// The child died on its own: reap it, reclaim, and hard-stop the slot.
    fn mark_dead(&mut self) {
        self.kill_reap_reclaim();
        self.slot_state = SlotState::Stopped {
            reason: StopReason::ProcessDied,
        };
    }
}

impl CyclicSlot for ProcSlot {
    fn init(&mut self) {
        self.ctl.request(WorkerState::InitReq);
        if self.ctl.wait_state(WorkerState::Ready, INIT_TIMEOUT).is_err() {
            self.mark_dead();
        }
    }

    fn step(&mut self, now: Timestamp) {
        if self.slot_state.is_stopped() {
            return;
        }
        match self.ctl.step(now, self.step_timeout) {
            // A stray `Done` folds to keep-running, as in `DlSlot::step`.
            StepOutcome::Acked(FswStatus::Running | FswStatus::Done) => {}
            StepOutcome::Acked(FswStatus::Panicked) => {
                // The worker-side DlSlot already destroyed the foreign state
                // (freeing its ring roles); the worker itself stays parked
                // serving acks until shutdown, so teardown stays symmetric.
                self.slot_state = SlotState::Stopped {
                    reason: StopReason::Panicked,
                };
            }
            StepOutcome::TimedOut => match self.child.try_wait() {
                // The worker is gone; the abandoned sequence never resolves.
                Ok(Some(_)) => self.mark_dead(),
                // Alive but late: telemetered, and the sequence protocol
                // self-heals (the worker serves only the newest doorbell).
                Ok(None) | Err(_) => self.timeouts += 1,
            },
        }
    }

    fn shutdown(&mut self) {
        if matches!(
            self.slot_state,
            SlotState::Stopped {
                reason: StopReason::ProcessDied
            }
        ) {
            return; // already reaped and reclaimed
        }
        self.ctl.request(WorkerState::ShutdownReq);
        let _ = self.ctl.wait_state(WorkerState::Done, SHUTDOWN_GRACE);
        // Reap within the grace window; then end it for certain either way.
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
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
}

impl Drop for ProcSlot {
    fn drop(&mut self) {
        // A clean shutdown already reaped (kill/wait/reclaim are idempotent);
        // this covers a coordinator dropped mid-run.
        self.kill_reap_reclaim();
    }
}
