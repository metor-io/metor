//! The per-worker control block: one small mmap'd file through which the
//! host steps a process system in lockstep with the cycle.
//!
//! The block carries a lifecycle word, a doorbell/ack sequence pair, the
//! step's timestamp, and the last step's status, all atomics woken through
//! [`ring::wake`](metor_fsw_ring::wake). It follows the ring region's header
//! discipline: an immutable header (magic, version, arch tag) written once by
//! the host before the worker is spawned and validated on attach, then only
//! atomic words.
//!
//! The two halves are separate types so each side can only speak its own end
//! of the protocol: [`CtlHost`] creates the file, requests lifecycle
//! transitions, and rings [`CtlHost::step`]; [`CtlWorker`] attaches, reports
//! transitions, blocks in [`CtlWorker::next`], and acks with
//! [`CtlWorker::done`].
//!
//! # Lifecycle
//!
//! ```text
//! host                                   worker
//! ----                                   ------
//! create (state = Booting), spawn ---->  attach, map rings, dlopen, create
//! wait_state(Attached)        <--------  report(Attached)
//! request(InitReq)            -------->  wait_init
//! wait_state(Ready)           <--------  bind_init; report(Ready)
//! step(now, deadline)         -------->  next() -> Step { seq, now }
//!   … one fsw_execute …
//! (ack == seq)                <--------  done(seq, status)
//! request(ShutdownReq)        -------->  next() -> Shutdown
//! wait_state(Done)            <--------  shutdown/destroy; report(Done)
//! ```
//!
//! # Stepping and lateness
//!
//! [`CtlHost::step`] stores the timestamp, bumps the doorbell (the Release
//! pair that publishes the timestamp), wakes the worker, and waits for the
//! ack to reach the new sequence — bounded by a deadline, so a hung worker
//! costs a [`StepOutcome::TimedOut`] instead of a stalled cycle. Lateness is
//! self-healing: the worker always serves the *newest* doorbell value it
//! observes and acks exactly what it served, so a worker that slept through
//! doorbells simply skips them, and a late ack for an abandoned sequence is
//! superseded by the next step's wait.

use core::sync::atomic::{
    AtomicU32, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use std::path::Path;
use std::time::{Duration, Instant};

use metor_fsw_ring::wake;
use metor_proto::types::Timestamp;

use crate::abi::FswStatus;

/// Control-block magic, the bytes `b"MFC1"` read as a native `u32`.
const MAGIC: u32 = u32::from_ne_bytes(*b"MFC1");

/// Control-block layout version, bumped on any incompatible change. Blocks
/// are per-run IPC state created by the host that spawns the worker, so a
/// mismatch means mixed builds, never migration.
const VERSION: u16 = 1;

/// A tag identifying the architecture that wrote the block, the same scheme
/// as the ring region's: the byte pattern differs across endianness and the
/// low word carries the pointer width.
const fn arch_tag() -> u64 {
    ((0x0102_0304u32 as u64) << 32) | (core::mem::size_of::<usize>() as u64)
}

/// The identifying metadata at the start of the block. Written once by
/// [`CtlHost::create`] before the worker exists; nothing mutates it after,
/// which is what makes the plain read in [`CtlWorker::attach`] sound.
#[repr(C, align(8))]
struct CtlHeader {
    magic: u32,
    version: u16,
    _pad: u16,
    arch_tag: u64,
}

/// The whole shared block: the immutable header plus the atomic words both
/// sides coordinate through. `#[repr(C)]` fixes the field offsets; it is
/// never parsed from bytes, only pointer-cast over the mapping.
#[repr(C, align(8))]
struct CtlBlock {
    header: CtlHeader,
    /// The lifecycle word, holding a [`WorkerState`] discriminant. Doubles as
    /// its own futex word.
    state: AtomicU32,
    /// The step sequence, host-incremented. Doubles as the worker's futex
    /// word; lifecycle requests wake it too, so a worker parked here observes
    /// a shutdown.
    doorbell: AtomicU32,
    /// The last sequence the worker completed. The host's futex word.
    ack: AtomicU32,
    /// The last step's raw [`FswStatus`] word — untrusted, folded through
    /// [`FswStatus::from_raw`] on the host side. While `state` is
    /// [`Failed`](WorkerState::Failed) it instead carries the worker's
    /// failure code.
    status: AtomicU32,
    /// The step's [`Timestamp`] tick as a raw `u64`, published by the
    /// doorbell's Release/Acquire pair.
    now: AtomicU64,
}

/// The file is one block, padded to a cache line.
const CTL_FILE_SIZE: usize = 64;
const _: () = assert!(core::mem::size_of::<CtlBlock>() <= CTL_FILE_SIZE);

/// The worker lifecycle, held in [`CtlBlock::state`].
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// Host created the block; the worker has not reported yet.
    Booting = 0,
    /// Worker mapped its rings, opened the artifact, and ran `fsw_create`.
    Attached = 1,
    /// Host asks the worker to bind and init.
    InitReq = 2,
    /// Worker ran `fsw_bind_init`; ready to step.
    Ready = 3,
    /// Host asks the worker to shut down and exit.
    ShutdownReq = 4,
    /// Worker ran `fsw_shutdown`/`fsw_destroy` and is exiting cleanly.
    Done = 5,
    /// Worker-side failure; the `status` word carries its code.
    Failed = 6,
}

impl WorkerState {
    fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => WorkerState::Booting,
            1 => WorkerState::Attached,
            2 => WorkerState::InitReq,
            3 => WorkerState::Ready,
            4 => WorkerState::ShutdownReq,
            5 => WorkerState::Done,
            6 => WorkerState::Failed,
            _ => return None,
        })
    }
}

/// A control-block create/attach or protocol failure.
#[derive(Debug, thiserror::Error)]
pub enum CtlError {
    #[error("control block I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The mapped file is not a compatible control block (wrong magic,
    /// version, architecture, or size).
    #[error("control block invalid: {0}")]
    BadHeader(&'static str),
    /// The worker latched [`WorkerState::Failed`]; the code is what it left
    /// in the status word.
    #[error("worker reported failure (code {0})")]
    WorkerFailed(u32),
    /// The worker never reached the awaited state. Carries the state it was
    /// last seen in (`None` for a value outside the enum, i.e. corruption).
    #[error("timed out waiting for worker state {want:?} (last seen {seen:?})")]
    StateTimeout {
        want: WorkerState,
        seen: Option<WorkerState>,
    },
}

/// How one [`CtlHost::step`] resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// The worker completed this sequence; here is its status.
    Acked(FswStatus),
    /// The deadline lapsed first. The worker may still be computing — the
    /// caller decides between "slow" and "dead" (e.g. by `try_wait`ing the
    /// child).
    TimedOut,
}

/// What [`CtlWorker::next`] told the worker to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerCmd {
    /// Run one step for `seq` at time `now`, then [`CtlWorker::done`].
    Step { seq: u32, now: Timestamp },
    /// Shut down, report [`WorkerState::Done`], and exit.
    Shutdown,
}

/// Map a control file and hand back its base as a block reference.
///
/// # Safety
/// The mapping must be at least [`CTL_FILE_SIZE`] bytes (checked by both
/// constructors) and, on the attach side, header-validated. Every mutable
/// field is atomic and the header is written before the worker exists, so a
/// shared `&CtlBlock` over the live mapping is sound — the same argument as
/// the ring's `Control`.
fn block_of(map: &memmap2::MmapMut) -> &CtlBlock {
    debug_assert!(map.len() >= CTL_FILE_SIZE);
    // SAFETY: see above; mmap bases are page-aligned, far beyond align(8).
    unsafe { &*(map.as_ptr() as *const CtlBlock) }
}

// ---------------------------------------------------------------------------
// Host half
// ---------------------------------------------------------------------------

/// The host's end of one worker's control block. Created before the worker
/// is spawned; dropped after the worker is dead or done.
pub struct CtlHost {
    map: memmap2::MmapMut,
}

impl CtlHost {
    /// Create the control file at `path` (truncating any leftover), sized and
    /// initialized to `state = Booting`, ready for the worker to attach.
    pub fn create(path: &Path) -> Result<Self, CtlError> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(CTL_FILE_SIZE as u64)?;
        // SAFETY: mapping a file we just sized; held exclusively until spawn.
        let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        // SAFETY: fresh, exclusively-owned mapping of CTL_FILE_SIZE bytes; no
        // other thread or process can observe these writes yet.
        unsafe {
            (map.as_ptr() as *mut CtlBlock).write(CtlBlock {
                header: CtlHeader {
                    magic: MAGIC,
                    version: VERSION,
                    _pad: 0,
                    arch_tag: arch_tag(),
                },
                state: AtomicU32::new(WorkerState::Booting as u32),
                doorbell: AtomicU32::new(0),
                ack: AtomicU32::new(0),
                status: AtomicU32::new(0),
                now: AtomicU64::new(0),
            });
        }
        Ok(Self { map })
    }

    fn block(&self) -> &CtlBlock {
        block_of(&self.map)
    }

    /// Request a lifecycle transition, waking a worker parked on either word
    /// (a worker in its step loop parks on the doorbell, not the state).
    pub fn request(&self, state: WorkerState) {
        let b = self.block();
        b.state.store(state as u32, Release);
        wake::wake_all(&b.state);
        wake::wake_all(&b.doorbell);
    }

    /// The worker's current lifecycle word, without waiting. `None` for a
    /// value outside the enum (a corrupt or foreign block). The restart
    /// machine polls this once per cycle instead of blocking the loop.
    pub fn state(&self) -> Option<WorkerState> {
        WorkerState::from_raw(self.block().state.load(Acquire))
    }

    /// Wait until the worker reports `want`, a failure, or the timeout.
    pub fn wait_state(&self, want: WorkerState, timeout: Duration) -> Result<(), CtlError> {
        let b = self.block();
        let start = Instant::now();
        loop {
            let raw = b.state.load(Acquire);
            if raw == want as u32 {
                return Ok(());
            }
            if raw == WorkerState::Failed as u32 {
                return Err(CtlError::WorkerFailed(b.status.load(Acquire)));
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Err(CtlError::StateTimeout {
                    want,
                    seen: WorkerState::from_raw(raw),
                });
            }
            wake::wait_timeout(&b.state, raw, timeout - elapsed);
        }
    }

    /// Ring one step: publish `now`, bump the doorbell, and wait for the ack
    /// — at most `deadline`.
    pub fn step(&self, now: Timestamp, deadline: Duration) -> StepOutcome {
        let b = self.block();
        b.now.store(now.0 as u64, Relaxed);
        // The Release RMW publishes the `now` store to the worker's Acquire
        // doorbell load.
        let seq = b.doorbell.fetch_add(1, Release).wrapping_add(1);
        wake::wake_one(&b.doorbell);
        let start = Instant::now();
        loop {
            let ack = b.ack.load(Acquire);
            if ack == seq {
                // The ack's Release/Acquire pair publishes the status store.
                return StepOutcome::Acked(FswStatus::from_raw(b.status.load(Acquire)));
            }
            let elapsed = start.elapsed();
            if elapsed >= deadline {
                return StepOutcome::TimedOut;
            }
            wake::wait_timeout(&b.ack, ack, deadline - elapsed);
        }
    }
}

// ---------------------------------------------------------------------------
// Worker half
// ---------------------------------------------------------------------------

/// The worker's end of the control block. Attached from the file path the
/// launch manifest names, after the host created it.
pub struct CtlWorker {
    map: memmap2::MmapMut,
}

impl CtlWorker {
    /// Map the control file and validate its header.
    pub fn attach(path: &Path) -> Result<Self, CtlError> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        // SAFETY: the host created and sized this file before spawning us;
        // the header check below rejects anything else.
        let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        if map.len() < CTL_FILE_SIZE {
            return Err(CtlError::BadHeader("file shorter than a control block"));
        }
        // SAFETY: in-bounds (checked above); the header was written before
        // this process was spawned, so a plain read cannot race it.
        let hdr = unsafe { (map.as_ptr() as *const CtlHeader).read() };
        if hdr.magic != MAGIC {
            return Err(CtlError::BadHeader("wrong magic"));
        }
        if hdr.version != VERSION {
            return Err(CtlError::BadHeader("incompatible version"));
        }
        if hdr.arch_tag != arch_tag() {
            return Err(CtlError::BadHeader("architecture mismatch"));
        }
        Ok(Self { map })
    }

    fn block(&self) -> &CtlBlock {
        block_of(&self.map)
    }

    /// Report a lifecycle transition and wake the host.
    pub fn report(&self, state: WorkerState) {
        let b = self.block();
        b.state.store(state as u32, Release);
        wake::wake_all(&b.state);
    }

    /// Latch [`WorkerState::Failed`] with a failure code for the host's
    /// diagnostics (the code rides the status word).
    pub fn fail(&self, code: u32) {
        let b = self.block();
        b.status.store(code, Release);
        b.state.store(WorkerState::Failed as u32, Release);
        wake::wake_all(&b.state);
    }

    /// Park until the host requests init (`true`) or shutdown (`false`).
    pub fn wait_init(&self) -> bool {
        let b = self.block();
        loop {
            let raw = b.state.load(Acquire);
            if raw == WorkerState::InitReq as u32 {
                return true;
            }
            if raw == WorkerState::ShutdownReq as u32 {
                return false;
            }
            wake::wait(&b.state, raw);
        }
    }

    /// Park until there is something to do: a doorbell newer than
    /// `last_seq`, or a shutdown request. Always serves the *newest*
    /// doorbell value, so missed cycles are skipped, not replayed.
    pub fn next(&self, last_seq: u32) -> WorkerCmd {
        let b = self.block();
        loop {
            if b.state.load(Acquire) == WorkerState::ShutdownReq as u32 {
                return WorkerCmd::Shutdown;
            }
            let seq = b.doorbell.load(Acquire);
            if seq != last_seq {
                // The doorbell's Acquire pairs with the host's Release RMW,
                // publishing the `now` for this (or a newer) sequence.
                let now = Timestamp(b.now.load(Relaxed) as i64);
                return WorkerCmd::Step { seq, now };
            }
            wake::wait(&b.doorbell, seq);
        }
    }

    /// Ack sequence `seq` with the step's status and wake the host.
    pub fn done(&self, seq: u32, status: FswStatus) {
        let b = self.block();
        b.status.store(status as u32, Release);
        // The ack Release publishes the status store to the host's Acquire
        // ack load.
        b.ack.store(seq, Release);
        wake::wake_one(&b.ack);
    }
}
