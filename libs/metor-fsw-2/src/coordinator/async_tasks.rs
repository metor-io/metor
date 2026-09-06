//! Async task startup barriers and cooperative shutdown.

use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicUsize,
    Ordering::{Acquire, Release},
};
use std::time::{Duration, Instant};

use metor_fsw_2_core::StatusPort;
use metor_fsw_2_core::log::LogLevel;
use stellarator::JoinHandle;
use stellarator::sync::WaitQueue;

use super::Coordinator;
use crate::async_system::{AsyncContext, AsyncSystem};

const JOIN_TIMEOUT: Duration = Duration::from_millis(20);

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
pub(super) struct AsyncSlot<S: AsyncSystem> {
    pub(super) system: S,
    pub(super) input: S::Input,
    pub(super) output: S::Output,
    pub(super) status: StatusPort,
}

impl<S> AsyncLauncher for AsyncSlot<S>
where
    S: AsyncSystem + 'static,
    S::Input: 'static,
    S::Output: 'static,
{
    fn launch(self: Box<Self>, ctx: LaunchCtx) -> JoinHandle<()> {
        let mut slot = *self;
        stellarator::spawn(async move {
            // Init inside the task (the only owner of the bundle), then signal
            // readiness and hold at the go-gate until every system's init is done.
            slot.system.init(&mut slot.output);
            ctx.ready_count.fetch_add(1, Release);
            ctx.ready.wake_all();
            let _ = ctx.go.wait_for(|| ctx.go_flag.load(Acquire)).await;
            let mut context = AsyncContext {
                cancel: ctx.cancel,
                status: slot.status,
            };
            slot.system
                .run(&mut context, &mut slot.input, &mut slot.output)
                .await;
            slot.system.shutdown(&mut slot.output);
        })
    }
}

/// Cancels the task on drop, including when the coordinator is dropped mid-run.
pub(super) struct AsyncTask {
    pub(super) name: String,
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
pub(super) struct PendingAsync {
    pub(super) name: String,
    pub(super) launcher: Box<dyn AsyncLauncher>,
}

impl Coordinator {
    /// Complete all initialization before releasing tasks into their run loops.
    pub(super) async fn start(&mut self) -> Vec<AsyncTask> {
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
        for e in &mut self.cyclic {
            e.slot.init();
        }

        // Release the async tasks into their run loops.
        go_flag.store(true, Release);
        go.wake_all();
        tasks
    }

    pub(super) async fn stop_async(&mut self, tasks: Vec<AsyncTask>) {
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
                self.coord_log.fault(
                    LogLevel::Error,
                    "async_shutdown_timeout",
                    "async system exceeded shutdown deadline",
                    &[("system", &task.name)],
                );
                tracing::error!(system = %task.name, "async system exceeded shutdown deadline");
                let _ = handle.0.cancel();
            }
            let _ = handle.await;
        }
    }
}
