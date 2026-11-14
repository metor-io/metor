//! Threads the db owns for the background work of its components and message
//! logs: draining their WALs into storage, and sealing the nodes their writers
//! roll past. The work runs here rather than on the thread that created a
//! component, so it outlives the connection that announced it.

use std::{
    future::Future,
    mem,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use stellarator::sync::WaitQueue;
use tracing::error;

use crate::{Component, Error, msg_log::MsgLog};

/// A component or message log with WAL data to persist.
pub(crate) enum Pending {
    Component(Component),
    MsgLog(MsgLog),
}

impl Pending {
    fn persist(self) {
        // Cleared before draining, so a push that lands after the drain
        // queues the WAL again.
        let result = match self {
            Pending::Component(component) => {
                component.queued.swap(false, Ordering::AcqRel);
                component.flush_pending()
            }
            Pending::MsgLog(log) => {
                log.queued.swap(false, Ordering::AcqRel);
                log.flush_pending()
            }
        };
        if let Err(err) = result {
            error!(?err, "failed to persist wal data");
        }
    }
}

/// How the persist thread lets pushes gather once it has work. A WAL drained
/// the moment it has a sample is queued again, and the thread woken again, by
/// the producer's next push; one drain per window spares a busy producer both.
///
/// The window delays what readers see by as much, and a WAL holds 512 samples,
/// so it suits windows well under a stream tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gather {
    /// Sleeps out the window, so the thread idles between batches. A sleep
    /// overshoots by the timer's slack, tens of microseconds or more.
    Sleep(Duration),
    /// Yields in a loop for the window: the shortest windows hold, and the
    /// thread takes a core under steady load.
    Spin(Duration),
}

impl Gather {
    fn wait(self) {
        match self {
            Gather::Sleep(window) if !window.is_zero() => thread::sleep(window),
            Gather::Sleep(_) => {}
            Gather::Spin(window) => {
                let gathered = Instant::now() + window;
                while Instant::now() < gathered {
                    thread::yield_now();
                }
            }
        }
    }
}

impl Default for Gather {
    /// Idle CPU over latency: an interactive host such as the panel leaves
    /// most of its time between pushes.
    fn default() -> Self {
        Gather::Sleep(Duration::from_millis(1))
    }
}

/// The WALs waiting for the persist thread, which blocks on `ready` rather
/// than running an executor: persisting is synchronous, and a condvar wakes
/// it from any thread.
#[derive(Default)]
struct PersistQueue {
    state: Mutex<PersistState>,
    ready: Condvar,
}

#[derive(Default)]
struct PersistState {
    pending: Vec<Pending>,
    gather: Gather,
    closed: bool,
}

impl PersistQueue {
    fn lock(&self) -> MutexGuard<'_, PersistState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Persists what is queued until the queue closes.
    fn run(&self) {
        let mut state = self.lock();
        loop {
            if state.closed {
                return;
            }
            if state.pending.is_empty() {
                state = self
                    .ready
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            let gather = state.gather;
            drop(state);
            gather.wait();
            state = self.lock();
            let batch = mem::take(&mut state.pending);
            drop(state);
            for pending in batch {
                pending.persist();
            }
            state = self.lock();
        }
    }

    fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_one();
    }
}

/// Hands components and message logs with new WAL data to the persist thread.
#[derive(Clone)]
pub(crate) struct Persister(Arc<PersistQueue>);

impl Persister {
    /// Queues a WAL for persisting unless its `queued` flag says it already
    /// is. Returns whether the persist thread needs a [`wake`](Self::wake),
    /// which a caller pushing many WALs at once sends after the last.
    pub(crate) fn queue(&self, queued: &AtomicBool, pending: impl FnOnce() -> Pending) -> bool {
        if queued.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut state = self.0.lock();
        state.pending.push(pending());
        state.pending.len() == 1
    }

    pub(crate) fn wake(&self) {
        self.0.ready.notify_one();
    }

    /// Takes effect from the next batch.
    pub(crate) fn set_gather(&self, gather: Gather) {
        self.0.lock().gather = gather;
    }
}

type Job = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// How often the seal thread checks for jobs and its close without a wake.
/// A wake from another thread can go missing, and this bounds what that
/// costs: a delayed spawn, rather than a db that never finishes dropping.
const SEAL_POLL: Duration = Duration::from_millis(100);

/// Tasks handed from any thread to the seal thread's executor.
struct Jobs {
    queue: Mutex<Vec<Job>>,
    waker: WaitQueue,
    closed: AtomicBool,
}

impl Jobs {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::new()),
            waker: WaitQueue::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// Spawns what is queued until the queue closes.
    async fn run(&self) {
        while !self.closed.load(Ordering::Acquire) {
            let woken = self
                .waker
                .wait_for(|| self.closed.load(Ordering::Acquire) || !self.lock().is_empty());
            let woken = async {
                let _ = woken.await;
            };
            futures_lite::future::or(woken, stellarator::sleep(SEAL_POLL)).await;
            for job in mem::take(&mut *self.lock()) {
                stellarator::spawn(job());
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Job>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.waker.wake_all();
    }
}

/// Spawns tasks onto the seal thread from any thread.
#[derive(Clone)]
pub(crate) struct Spawner(Arc<Jobs>);

impl Spawner {
    pub(crate) fn spawn<F: Future<Output = ()> + 'static>(
        &self,
        task: impl FnOnce() -> F + Send + 'static,
    ) {
        self.0.lock().push(Box::new(move || Box::pin(task())));
        self.0.waker.wake_all();
    }
}

/// Handles to the db's threads, for the components and logs it creates.
#[derive(Clone)]
pub struct Background {
    pub(crate) persister: Persister,
    /// Runs each time series' lifecycle task, whose seals checksum and fsync
    /// off the persist thread.
    pub(crate) sealer: Spawner,
}

/// The db's threads, closed and joined on drop.
pub struct Workers {
    background: Background,
    threads: Vec<thread::JoinHandle<()>>,
}

impl Workers {
    pub fn new() -> Result<Self, Error> {
        let persist = Arc::new(PersistQueue::default());
        let seal = Arc::new(Jobs::new());
        let threads = vec![
            thread::Builder::new()
                .name("metor-db-persist".into())
                .spawn({
                    let persist = persist.clone();
                    move || persist.run()
                })?,
            thread::Builder::new().name("metor-db-seal".into()).spawn({
                let seal = seal.clone();
                move || stellarator::run(|| async move { seal.run().await })
            })?,
        ];
        Ok(Self {
            background: Background {
                persister: Persister(persist),
                sealer: Spawner(seal),
            },
            threads,
        })
    }

    pub fn background(&self) -> &Background {
        &self.background
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.background.persister.0.close();
        self.background.sealer.0.close();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Threads shared by the tests that create components outside a db.
#[cfg(test)]
pub(crate) fn test_background() -> &'static Background {
    static WORKERS: std::sync::OnceLock<Workers> = std::sync::OnceLock::new();
    WORKERS
        .get_or_init(|| Workers::new().expect("spawning db threads"))
        .background()
}
