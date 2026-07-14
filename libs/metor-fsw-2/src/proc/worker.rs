//! The worker process: the other end of `docs/process-systems.md`.
//!
//! A worker is not a separate binary — it is the **host executable
//! re-executed** with [`WORKER_ENV`] naming a postcard [`WorkerManifest`].
//! [`worker_entry`] is the guard that routes such a child into the worker
//! instead of the application's own `main`.
//!
//! The manifest selects one of two modes:
//!
//! - **Describe**: dlopen the artifact, run `fsw_pack_describe`, write the
//!   raw postcard pack-manifest bytes to the output file, exit. This is what
//!   keeps the host free of foreign code — it decodes these bytes instead of
//!   loading the object itself, and one describe covers every entry the
//!   artifact exports.
//! - **Run**: attach the control block and every ring file, open the pack
//!   ([`DlPack::open`](crate::dl::DlPack), which runs the crate's `pack()` in
//!   this process), select the manifest's entry by name, and drive an
//!   ordinary [`DlSlot`](crate::dl) through the [`ctl`](super::ctl)
//!   lifecycle — `fsw_pack_create` at attach, `fsw_pack_bind_init` on the
//!   init request, one `fsw_pack_execute` per doorbell, and
//!   `fsw_pack_shutdown`/`fsw_pack_destroy` on the shutdown request. A
//!   [`RunMode`] on the manifest picks the step fold: `Cyclic` for a
//!   steady-state system, `Sequence` for a process slot's occupant.
//!
//! Everything downstream of the manifest reuses the dl machinery verbatim:
//! the ring files attach as regions, regions become positional
//! [`FswRing`](crate::abi::FswRing) handles, and the slot binds exactly as
//! it would in-process. The maps outlive the slot (drop order below), so the
//! dl teardown-ordering contract holds unchanged.

use std::path::{Path, PathBuf};

use metor_fsw_ring::RingBuffer;
use serde::{Deserialize, Serialize};

use crate::abi::{FSW_ABI_VERSION, FswRing, FswStatus, ROLE_INPUT, ROLE_OUTPUT};
use crate::coordinator::{CyclicSlot, StopReason};

use super::ctl::{CtlWorker, WorkerCmd, WorkerState};

/// The environment variable that routes a re-executed host binary into the
/// worker: its value is the path of a postcard-encoded [`WorkerManifest`].
pub const WORKER_ENV: &str = "METOR_FSW_WORKER";

/// The launch manifest the host writes for each worker spawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkerManifest {
    /// Describe `artifact` and write the raw manifest bytes to `out`.
    Describe {
        /// The pack cdylib to describe.
        artifact: PathBuf,
        /// Where to write the postcard [`PackManifest`](crate::abi::PackManifest) bytes.
        out: PathBuf,
    },
    /// Drive one system instance until told to shut down.
    Run {
        /// The host's [`FSW_ABI_VERSION`]; a mismatched worker build fails
        /// cleanly before touching the artifact.
        abi_version: u32,
        /// How the step loop folds the system's status (see [`RunMode`]).
        mode: RunMode,
        /// The instance name (health/status identity inside the `.so`).
        instance: String,
        /// The pack entry to instantiate from the artifact's manifest.
        system: String,
        /// The pack cdylib to load.
        artifact: PathBuf,
        /// Canonical postcard `Params` bytes for `fsw_create`.
        params: Vec<u8>,
        /// The control-block file ([`CtlWorker::attach`]).
        ctl: PathBuf,
        /// Input ring files, **in descriptor order** — each an upstream
        /// producer's output ring (the `bind_dl` positional contract).
        inputs: Vec<PathBuf>,
        /// Output ring files, in descriptor order, this system's own.
        outputs: Vec<PathBuf>,
    },
}

/// How a run worker's step loop drives its system, selected by the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunMode {
    /// A steady-state cyclic system: each doorbell is one
    /// [`DlSlot::step`](crate::coordinator::CyclicSlot::step), and a stray
    /// `Done` folds to keep-running exactly as it would in-process.
    Cyclic,
    /// A process slot's sequence occupant: each doorbell is one
    /// [`DlSlot::step_seq`](crate::dl), whose raw status
    /// (`Running`/`Done`/`Panicked`) crosses the ack verbatim so the
    /// host-side runner can refine the terminal. `Done` is latched
    /// worker-side — a `Ready` future is never polled again, whatever the
    /// host rings.
    Sequence,
}

/// Worker failure codes, latched via [`CtlWorker::fail`] so the host's
/// diagnostics can name what died before `Ready`.
pub(crate) mod fail_code {
    /// The manifest's `abi_version` does not match this worker build.
    pub const ABI_VERSION: u32 = 1;
    /// A ring file failed to attach.
    pub const RINGS: u32 = 2;
    /// The artifact failed to load or describe ([`DlError`](crate::dl::DlError)).
    pub const ARTIFACT: u32 = 3;
    /// `fsw_create` panicked inside the artifact.
    pub const CREATE: u32 = 4;
}

/// Whether this binary's `main` called [`worker_entry`] — that is, whether it
/// can safely be re-executed as a worker. Consulted by the build driver's
/// manifest-sidecar step, which prefers a describe worker but must not
/// re-execute an unguarded binary (a libtest harness would run its tests).
static GUARD_INSTALLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether [`worker_entry`] has run in this process, making worker re-exec
/// available.
#[cfg(feature = "wiring")]
pub(crate) fn worker_guard_installed() -> bool {
    GUARD_INSTALLED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Route a re-executed host binary into the worker. **Call this first in
/// `main`** of any binary that runs process systems — the `metor-fsw` CLI
/// does; an application embedding the framework must do so itself. When
/// [`WORKER_ENV`] is unset this is one environment read and returns
/// immediately; when set, the process *is* a worker: it runs to completion
/// and exits, never returning to the caller's `main`.
pub fn worker_entry() {
    GUARD_INSTALLED.store(true, core::sync::atomic::Ordering::Relaxed);
    let Some(path) = std::env::var_os(WORKER_ENV) else {
        return;
    };
    std::process::exit(run_worker(Path::new(&path)));
}

/// Decode the manifest and run the selected mode, returning the exit code.
fn run_worker(manifest_path: &Path) -> i32 {
    let manifest = match std::fs::read(manifest_path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            postcard::from_bytes::<WorkerManifest>(&bytes).map_err(|e| e.to_string())
        }) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "metor-fsw worker: cannot read manifest `{}`: {e}",
                manifest_path.display()
            );
            return 1;
        }
    };
    match manifest {
        WorkerManifest::Describe { artifact, out } => run_describe(&artifact, &out),
        WorkerManifest::Run {
            abi_version,
            mode,
            instance,
            system,
            artifact,
            params,
            ctl,
            inputs,
            outputs,
        } => run_system(
            abi_version,
            mode,
            instance,
            &system,
            &artifact,
            &params,
            &ctl,
            &inputs,
            &outputs,
        ),
    }
}

/// Describe mode: the artifact's raw describe bytes, verbatim, to `out`.
fn run_describe(artifact: &Path, out: &Path) -> i32 {
    match crate::dl::describe_raw(artifact) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(out, &bytes) {
                eprintln!(
                    "metor-fsw worker: cannot write descriptor to `{}`: {e}",
                    out.display()
                );
                return 1;
            }
            0
        }
        Err(e) => {
            eprintln!(
                "metor-fsw worker: describe of `{}` failed: {e}",
                artifact.display()
            );
            1
        }
    }
}

/// Run mode: attach everything, then drive the slot through the control
/// block's lifecycle.
#[allow(clippy::too_many_arguments)]
fn run_system(
    abi_version: u32,
    mode: RunMode,
    instance: String,
    system: &str,
    artifact: &Path,
    params: &[u8],
    ctl_path: &Path,
    input_paths: &[PathBuf],
    output_paths: &[PathBuf],
) -> i32 {
    // The control block attaches first so every later failure can be
    // reported through it instead of only to stderr.
    let ctl = match CtlWorker::attach(ctl_path) {
        Ok(ctl) => ctl,
        Err(e) => {
            eprintln!(
                "metor-fsw worker: cannot attach control block `{}`: {e}",
                ctl_path.display()
            );
            return 1;
        }
    };
    // Report the failure code and exit nonzero; used for every pre-Ready
    // failure below.
    let fail = |code: u32, msg: String| -> i32 {
        eprintln!("metor-fsw worker: {msg}");
        ctl.fail(code);
        1
    };

    if abi_version != FSW_ABI_VERSION {
        return fail(
            fail_code::ABI_VERSION,
            format!("manifest ABI {abi_version} != worker build ABI {FSW_ABI_VERSION}"),
        );
    }

    // Attach every ring file. The `RingBuffer`s own the mappings and must
    // outlive the slot (whose ports hold non-owning attaches into them);
    // they live to the end of this function, the slot is dropped explicitly
    // before returning.
    let attach_all = |paths: &[PathBuf]| -> Result<Vec<RingBuffer>, String> {
        paths
            .iter()
            // SAFETY: each path names a ring region the host created for
            // this worker and keeps alive until it reaps us; attach
            // validates the header.
            .map(|p| unsafe {
                RingBuffer::attach_mmap(p).map_err(|e| format!("ring `{}`: {e}", p.display()))
            })
            .collect()
    };
    let input_rings = match attach_all(input_paths) {
        Ok(rings) => rings,
        Err(e) => return fail(fail_code::RINGS, e),
    };
    let output_rings = match attach_all(output_paths) {
        Ok(rings) => rings,
        Err(e) => return fail(fail_code::RINGS, e),
    };
    let handle = |rb: &RingBuffer, role: u8| {
        let (base, len) = rb.region();
        FswRing { base, len, role }
    };
    let inputs: Vec<FswRing> = input_rings.iter().map(|r| handle(r, ROLE_INPUT)).collect();
    let outputs: Vec<FswRing> = output_rings.iter().map(|r| handle(r, ROLE_OUTPUT)).collect();

    let dl = match crate::dl::DlPack::open(artifact).and_then(|pack| pack.system(system)) {
        Ok(dl) => dl,
        Err(e) => {
            return fail(
                fail_code::ARTIFACT,
                format!("cannot load `{}` from `{}`: {e}", system, artifact.display()),
            );
        }
    };
    // The slot wants a `&'static str` identity; one leak per worker process.
    let name: &'static str = Box::leak(instance.into_boxed_str());
    // SAFETY: every region named in `inputs`/`outputs` is held live by the
    // `RingBuffer`s above until after the slot is dropped below.
    // A sequence-mode worker drives a slot occupant, so the entry binds the
    // occupant tail; a cyclic worker is an ordinary wired mount.
    let mount = match mode {
        RunMode::Cyclic => crate::Mount::Wired,
        RunMode::Sequence => crate::Mount::SlotOccupant,
    };
    let mut slot = unsafe { dl.make_slot(params, inputs, outputs, name, mount) };
    if slot.state().is_stopped() {
        return fail(fail_code::CREATE, "fsw_create panicked in the artifact".into());
    }

    ctl.report(WorkerState::Attached);
    if ctl.wait_init() {
        // Bind the rings and run `System::init` inside the `.so`, under the
        // coordinator's init barrier on the host side.
        slot.init();
        ctl.report(WorkerState::Ready);

        let mut last = 0u32;
        while let WorkerCmd::Step { seq, now } = ctl.next(last) {
            // Both modes implement the panic policy inside the slot: a caught
            // panic destroys the foreign state (releasing its ring roles) and
            // latches the stopped slot state, and further steps early-return.
            // The worker keeps serving terminal acks until told to shut down,
            // so teardown stays symmetric with the healthy path — a `Done`
            // occupant likewise holds its ring roles until shutdown, and
            // `step_seq`'s latch guarantees its `Ready` future is never
            // polled again however long the host keeps ringing.
            let status = match mode {
                RunMode::Cyclic => {
                    slot.step(now);
                    match slot.state().stop_reason() {
                        Some(StopReason::Panicked) => FswStatus::Panicked,
                        _ => FswStatus::Running,
                    }
                }
                // The raw status crosses the ack verbatim; the host-side
                // runner refines `Done` with the occupant's own outcome.
                RunMode::Sequence => slot.step_seq(now),
            };
            ctl.done(seq, status);
            last = seq;
        }
    }
    slot.shutdown();
    // `fsw_destroy` (and the `.so` unload) before the ring maps drop.
    drop(slot);
    drop(dl);
    ctl.report(WorkerState::Done);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest round-trips through postcard: describe, and a run in
    /// each [`RunMode`].
    #[test]
    fn manifest_roundtrip() {
        for m in [
            WorkerManifest::Describe {
                artifact: PathBuf::from("/a/lib.dylib"),
                out: PathBuf::from("/tmp/out.desc"),
            },
            WorkerManifest::Run {
                abi_version: FSW_ABI_VERSION,
                mode: RunMode::Cyclic,
                instance: "imu".into(),
                system: "sys".into(),
                artifact: PathBuf::from("/a/lib.dylib"),
                params: vec![1, 2, 3],
                ctl: PathBuf::from("/run/imu.ctl"),
                inputs: vec![PathBuf::from("/run/a.ring")],
                outputs: vec![PathBuf::from("/run/b.ring"), PathBuf::from("/run/c.ring")],
            },
            WorkerManifest::Run {
                abi_version: FSW_ABI_VERSION,
                mode: RunMode::Sequence,
                instance: "adcs.commissioning".into(),
                system: "sys".into(),
                artifact: PathBuf::from("/a/seq.dylib"),
                params: Vec::new(),
                ctl: PathBuf::from("/run/adcs.ctl"),
                inputs: vec![PathBuf::from("/run/a.ring"), PathBuf::from("/run/d.ring")],
                outputs: vec![PathBuf::from("/run/e.ring")],
            },
        ] {
            let bytes = postcard::to_allocvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<WorkerManifest>(&bytes).unwrap(), m);
        }
    }

    /// Without the env var, `worker_entry` is a no-op (this test surviving
    /// is the assertion), and a garbage manifest is a clean nonzero exit
    /// code from `run_worker` rather than a panic.
    #[test]
    fn entry_and_bad_manifest() {
        worker_entry();
        assert_eq!(run_worker(Path::new("/nonexistent/manifest")), 1);
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk");
        std::fs::write(&junk, [0xFFu8; 32]).unwrap();
        assert_eq!(run_worker(&junk), 1);
    }
}
