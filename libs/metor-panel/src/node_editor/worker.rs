//! Long-lived stellarator thread that runs every dynamic-node producer task.
//!
//! Why this exists: dynamic node constructors (`ops::clock::fixed_rate`,
//! `ops::generators::waveform`, `ops::persist::persist`, ...) all call
//! `stellarator::spawn` to launch their producer tasks. `stellarator::spawn`
//! reads a thread-local `Executor`. The panel's main thread runs gpui's
//! event loop and never installs a stellarator runtime, so spawned tasks
//! land on the default empty executor and are never polled. Clocks never
//! tick, generators never produce, the `persist` task never copies samples
//! into the DB component WAL — so no data shows up downstream.
//!
//! `DynamicWorker` owns its own OS thread that runs `stellarator::run(...)`.
//! Build closures are sent over a channel; the worker thread executes them
//! locally so any `stellarator::spawn` calls inside the constructor target
//! its own scheduler. The producer tasks then live and tick on that thread.
//!
//! Build is synchronous from the caller's perspective (gpui main) — we
//! recv-block on the reply. Builds are cheap (allocate ring + spawn task)
//! so the worker turns around in well under a frame even with the 2 ms
//! poll interval, and rebuilds are debounced 200 ms upstream.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use gpui::{App, Global};

use crate::dynamic::{BuildError, DynamicNode};

/// A heap-boxed build closure. `Send` so it can cross the channel; `'static`
/// so it can outlive the call frame.
pub type BuildClosure =
    Box<dyn FnOnce() -> Result<Arc<dyn DynamicNode>, BuildError> + Send + 'static>;

struct BuildJob {
    closure: BuildClosure,
    reply: Sender<Result<Arc<dyn DynamicNode>, BuildError>>,
}

/// Cheap clonable handle to the worker. `rebuild_into` takes one of these
/// instead of `&DynamicWorker` so the caller can clone it out of `cx`'s
/// global storage before borrowing other globals (e.g. `DynamicRegistry`)
/// mutably from the same `cx`.
#[derive(Clone)]
pub struct WorkerHandle {
    tx: Sender<BuildJob>,
    /// Disposal channel for `Arc<dyn DynamicNode>` whose strong count is
    /// about to go to 1 (then 0) on a non-worker thread. Routing them here
    /// guarantees the actual drop — and the cancellation of the underlying
    /// `JoinHandleDropGuard` — happens on the thread that owns the
    /// stellarator timer, which avoids the cross-thread spinlock deadlock
    /// in `maitake::time::Sleep::drop`.
    dispose_tx: Sender<Arc<dyn DynamicNode>>,
}

impl WorkerHandle {
    /// Run `closure` on the worker thread; block until its result returns.
    /// Used from gpui main during a debounced rebuild.
    pub fn run(&self, closure: BuildClosure) -> Result<Arc<dyn DynamicNode>, BuildError> {
        let (reply_tx, reply_rx) = bounded(1);
        if self
            .tx
            .send(BuildJob {
                closure,
                reply: reply_tx,
            })
            .is_err()
        {
            // Worker thread is gone — degrade gracefully.
            return Err(BuildError::ParentFailed);
        }
        reply_rx.recv().unwrap_or(Err(BuildError::ParentFailed))
    }

    /// Hand `arc` to the worker thread for disposal. Best-effort: if the
    /// worker is gone, we drop locally as a fallback.
    pub fn dispose(&self, arc: Arc<dyn DynamicNode>) {
        if let Err(crossbeam_channel::SendError(arc)) = self.dispose_tx.send(arc) {
            drop(arc);
        }
    }
}

pub struct DynamicWorker {
    handle: WorkerHandle,
    /// Hold the worker thread handle so it lives as long as the global.
    /// gpui doesn't surface a clean app-shutdown hook so we don't bother
    /// joining; the OS reaps the thread on process exit.
    _thread: stellarator::struc_con::Thread<Option<()>>,
}

impl Global for DynamicWorker {}

impl Default for DynamicWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicWorker {
    pub fn new() -> Self {
        let (tx, rx) = unbounded::<BuildJob>();
        let (dispose_tx, dispose_rx) = unbounded::<Arc<dyn DynamicNode>>();
        let thread = stellarator::struc_con::stellar(move || async move {
            run_worker(rx, dispose_rx).await;
        });
        Self {
            handle: WorkerHandle { tx, dispose_tx },
            _thread: thread,
        }
    }

    pub fn init(cx: &mut App) {
        cx.set_global(Self::new());
    }

    /// Borrow the cheap clonable handle. Clone it before borrowing other
    /// globals from the same `cx`.
    pub fn handle(&self) -> &WorkerHandle {
        &self.handle
    }
}

/// Single drainer for both build jobs and Arc disposals. Disposing here —
/// rather than letting the gpui main thread drop the Arc directly — keeps
/// `Sleep::drop` (and any other timer-touching destructor inside the
/// spawned task) on the thread that owns the timer's spinlock.
async fn run_worker(rx: Receiver<BuildJob>, dispose_rx: Receiver<Arc<dyn DynamicNode>>) {
    loop {
        while let Ok(job) = rx.try_recv() {
            let result = (job.closure)();
            // If the requester went away, drop the result silently.
            let _ = job.reply.send(result);
        }
        // Drain the graveyard. `drop(arc)` here triggers the
        // `JoinHandleDropGuard` inside `NodeImpl`, which calls `cancel()`
        // on the underlying maitake task — and `cancel()` on the same
        // thread the task was spawned on synchronously deallocates without
        // needing to fight the timer's spinlock.
        while let Ok(arc) = dispose_rx.try_recv() {
            drop(arc);
        }
        // Yield to the executor so the producer tasks we just spawned
        // actually get to run, and poll the inbox again shortly. Build
        // requests are rare (200 ms debounce) so the 2 ms re-poll is
        // invisible to the user.
        stellarator::sleep(Duration::from_millis(2)).await;
    }
}
