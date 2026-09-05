//! Runs dynamic-node constructors and producers on a dedicated stellarator
//! executor. Builds use a synchronous request/reply channel; node disposal
//! runs on the same thread to keep timer cancellation on its owning executor.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use gpui::{App, Global};

use crate::dynamic::{BuildError, DynamicNode};

/// A heap-boxed build closure. `Send` so it can cross the channel; `'static`
/// so it can outlive the call frame.
pub type BuildClosure =
    Box<dyn FnOnce() -> Result<Arc<dyn DynamicNode>, BuildError> + Send + 'static>;

/// A unit of work for the worker thread. The reply channel is closed over
/// rather than carried alongside, which is what lets one channel serve every
/// return type — a node graph builds nodes, a program builds nodes and the
/// cells that watch them, and the thread does not care which.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// Cheap clonable handle to the worker. `rebuild_into` takes one of these
/// instead of `&DynamicWorker` so the caller can clone it out of `cx`'s
/// global storage before borrowing other globals (e.g. `DynamicRegistry`)
/// mutably from the same `cx`.
#[derive(Clone)]
pub struct WorkerHandle {
    tx: Sender<Job>,
    /// Disposal channel for `Arc<dyn DynamicNode>` whose strong count is
    /// about to go to 1 (then 0) on a non-worker thread. Routing them here
    /// guarantees the actual drop — and the cancellation of the underlying
    /// `JoinHandleDropGuard` — happens on the thread that owns the
    /// stellarator timer, which avoids the cross-thread spinlock deadlock
    /// in `maitake::time::Sleep::drop`.
    dispose_tx: Sender<Arc<dyn DynamicNode>>,
}

impl WorkerHandle {
    /// Run `f` on the worker thread; block until it returns.
    ///
    /// `None` means the worker thread is gone, which is the only way this
    /// fails. Everything a caller wants to say about *its own* failure travels
    /// in `T`.
    pub fn call<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (reply_tx, reply_rx) = bounded(1);
        let job: Job = Box::new(move || {
            let _ = reply_tx.send(f());
        });
        if self.tx.send(job).is_err() {
            return None;
        }
        reply_rx.recv().ok()
    }

    /// Build one node on the worker thread. Used from gpui main during a
    /// debounced rebuild.
    pub fn run(&self, closure: BuildClosure) -> Result<Arc<dyn DynamicNode>, BuildError> {
        // Worker thread gone — degrade gracefully.
        self.call(closure).unwrap_or(Err(BuildError::ParentFailed))
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
    /// Detached on drop; producers run until process exit.
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
        let (tx, rx) = unbounded::<Job>();
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
async fn run_worker(rx: Receiver<Job>, dispose_rx: Receiver<Arc<dyn DynamicNode>>) {
    loop {
        // A job closes over its own reply channel; if the requester went
        // away, the result is dropped silently.
        while let Ok(job) = rx.try_recv() {
            job();
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
