use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    time::Duration,
};

#[cfg(not(target_os = "linux"))]
pub mod poll;
#[cfg(target_os = "linux")]
pub mod uring;

#[cfg(not(target_os = "linux"))]
type DefaultReactor = poll::PollingReactor;
#[cfg(target_os = "linux")]
type DefaultReactor = uring::UringReactor;

pub use stellarator_buf as buf;
pub mod fs;
pub mod io;
pub mod net;
pub mod os;
#[cfg(not(target_os = "windows"))]
pub mod serial;
pub mod struc_con;
pub mod time;
pub mod util;

mod noop_waker;

use maitake::{scheduler::ExternalWaker, task::TaskRef, time::Timer};
use pin_project::pin_project;
#[cfg(not(target_os = "linux"))]
pub(crate) use poll as reactor;
#[cfg(target_os = "linux")]
pub(crate) use uring as reactor;

pub use maitake::future::yield_now;
pub use maitake::sync;

#[cfg(feature = "derive")]
pub use stellarator_macros::*;

thread_local! {
    /// The current executor, which `spawn`, `sleep`, and IO use. The code that
    /// installs it keeps it alive until it restores the previous one.
    static EXEC: Cell<*const Executor> = const { Cell::new(std::ptr::null()) };
    /// The thread's default executor. `with` creates it on first use and `run`
    /// drops it.
    static OWN: Own = const { Own(RefCell::new(None)) };
    /// Set once this thread's thread-locals are being destroyed.
    static EXITING: Cell<bool> = const { Cell::new(false) };
}

/// Holds the thread's default executor.
struct Own(RefCell<Option<Rc<Executor>>>);

impl Drop for Own {
    /// Marks the thread as exiting first, so the executor's `Drop` runs no
    /// task code once other thread-locals, such as tracing's, may be gone.
    fn drop(&mut self) {
        EXITING.set(true);
        drop(self.0.take());
    }
}

/// Installs an executor in `EXEC` and restores the previous one on drop.
struct Installed(*const Executor);

impl Installed {
    fn new(exec: &Executor) -> Self {
        let exec: *const Executor = exec;
        Self(EXEC.try_with(|e| e.replace(exec)).unwrap_or(exec))
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        let _ = EXEC.try_with(|e| e.set(self.0));
    }
}

/// The executor's state after one [`Executor::turn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Turned {
    /// Whether a task is ready to run on the next turn.
    pub runnable: bool,
    /// Time until the timer's next deadline, on the executor's clock.
    pub next_deadline: Option<Duration>,
}

#[derive(Debug, thiserror::Error)]
#[cfg_attr(feature = "miette", derive(miette::Diagnostic))]
pub enum Error {
    #[error("submission queue full")]
    SubmissionQueueFull,
    #[error("executor already installed")]
    ExecutorAlreadyInstalled,
    #[error("io {0}")]
    Io(std::io::Error),
    #[error("invalid socket addr type")]
    InvalidSocketAddrType,
    #[error("invalid path")]
    InvalidPath,

    // ideally unreachable states
    #[error("completion state missing")]
    CompletionStateMissing,
    #[error("polled ignored completion")]
    PolledIgnoredCompletion,
    #[error("completion op code missing")]
    CompletionOpCodeMissing,

    #[error("buffer overflow")]
    BufferOverflow,
    #[error("end of file")]
    EOF,
    #[error("integer overflow")]
    IntegerOverflow,

    #[error("join failed")]
    JoinFailed,
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<rustix::io::Errno> for Error {
    fn from(err: rustix::io::Errno) -> Self {
        Error::Io(std::io::Error::from_raw_os_error(err.raw_os_error()))
    }
}

pub type BufResult<T, B> = (Result<T, Error>, B);

pub struct Executor {
    reactor: RefCell<DefaultReactor>,
    scheduler: maitake::scheduler::LocalScheduler,
    tasks: RefCell<Vec<TaskRef>>,
    timer: maitake::time::Timer,
}

impl Drop for Executor {
    /// Cancels every task and ticks once, so each task drops its future and any
    /// `Sleep` on this timer before the timer drops. Then cancels pending IO.
    /// Skips all of it while the thread exits.
    fn drop(&mut self) {
        if EXITING.try_with(Cell::get).unwrap_or(true) {
            return;
        }
        let this = &*self;
        let _installed = Installed::new(this);
        for task in this.tasks.borrow().iter() {
            task.cancel();
        }
        this.scheduler.tick();
        this.scheduler.cancel_all();
        this.tasks.borrow_mut().clear();
        let io = &mut this.reactor.borrow_mut().drain_io();
        io.cancel();
    }
}

pub trait Reactor {
    fn wait_for_io(&mut self, timeout: Option<Duration>) -> Result<(), Error>;
    fn process_io(&mut self) -> Result<(), Error>;

    type RemainingIo: IoStates;

    fn drain_io(&mut self) -> Self::RemainingIo;
    fn waker(&self) -> Waker;
    fn external_waker(&self) -> impl ExternalWaker;
}

pub trait IoStates {
    fn cancel(&mut self);
}

impl IoStates for () {
    fn cancel(&mut self) {}
}

/// Runs `func`'s future to completion on the thread's default executor, then
/// drops that executor. The next `run` creates a new one.
pub fn run<R, F>(func: impl FnOnce() -> F) -> R
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    let exec = Executor::own();
    let out = exec.run(func).unwrap();
    OWN.with(|o| o.0.borrow_mut().take());
    out
}

impl Executor {
    pub fn run<O, F>(&self, func: impl FnOnce() -> F) -> Result<O, Error>
    where
        F: Future<Output = O> + 'static,
        O: 'static,
    {
        let _installed = Installed::new(self);
        let future = func();
        let main_task = self.scheduler.spawn(future);
        self.track(main_task.task_ref());
        let mut main_task = pin!(main_task);
        let waker = self.reactor.borrow_mut().waker();
        let mut cx = Context::from_waker(&waker);
        let out = loop {
            self.timer.try_turn();
            self.reactor.borrow_mut().process_io()?;
            let tick = self.scheduler.tick();
            if let Poll::Ready(o) = main_task.as_mut().poll(&mut cx) {
                match o {
                    Ok(o) => {
                        if tick.has_remaining {
                            self.scheduler.cancel_all();
                        }
                        break o;
                    }
                    Err(err) if err.is_completed() => {}
                    Err(err) => {
                        panic!("main task join failed {err:?}");
                    }
                }
            }
            let turn = self.timer.try_turn();
            if !tick.has_remaining && turn.as_ref().map(|t| t.expired == 0).unwrap_or(true) {
                self.reactor
                    .borrow_mut()
                    .wait_for_io(turn.and_then(|turn| turn.time_to_next_deadline()))?;
            }
        };
        self.scheduler.cancel_all();
        let io = &mut self.reactor.borrow_mut().drain_io();
        io.cancel();
        Ok(out)
    }

    /// Records a task for `Drop` to cancel, and prunes finished ones.
    fn track(&self, task: TaskRef) {
        let mut tasks = self.tasks.borrow_mut();
        // Pruning only when full keeps `track` amortized O(1).
        if tasks.len() == tasks.capacity() {
            tasks.retain(|t| !t.is_complete());
        }
        tasks.push(task);
    }

    /// Creates an executor with its own reactor and the given clock.
    pub fn new(clock: time::Clock) -> Result<Self, Error> {
        let mut exec = Self::try_new()?;
        exec.timer = Timer::new(clock.maitake());
        Ok(exec)
    }

    /// Advances the timer, processes IO, and ticks the scheduler once, with
    /// `self` as the current executor. Never blocks and never moves the clock.
    pub fn turn(self: &Rc<Self>) -> Turned {
        let _installed = Installed::new(self);
        self.timer.try_turn();
        let _ = self.reactor.borrow_mut().process_io();
        let tick = self.scheduler.tick();
        let turn = self.timer.try_turn();
        let woke = turn.as_ref().is_some_and(|t| t.expired > 0);
        Turned {
            runnable: tick.has_remaining || woke,
            next_deadline: turn.and_then(|t| t.time_to_next_deadline()),
        }
    }

    /// Runs `f` with `self` as the current executor, so `spawn` and `sleep`
    /// inside it reach `self`.
    pub fn enter<T>(self: &Rc<Self>, f: impl FnOnce() -> T) -> T {
        let _installed = Installed::new(self);
        f()
    }

    /// Spawns a task on this executor.
    ///
    /// Drop every `JoinHandle` from this executor before the executor itself.
    pub fn spawn<F>(self: &Rc<Self>, f: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: Send + 'static,
    {
        let _installed = Installed::new(self);
        spawn(f)
    }

    /// Returns the thread's default executor, creating it if needed.
    fn own() -> Rc<Self> {
        OWN.with(|o| {
            o.0.borrow_mut()
                .get_or_insert_with(|| Rc::new(Self::default()))
                .clone()
        })
    }

    pub(crate) fn with<R>(f: impl for<'a> FnOnce(&'a Self) -> R) -> R {
        let mut exec = EXEC.with(Cell::get);
        let mut own = None;
        if exec.is_null() {
            let rc = own.insert(Self::own());
            exec = Rc::as_ptr(rc);
        }
        // SAFETY: the code that installed `exec` keeps it alive, and `own`
        // keeps the default executor alive for this call.
        f(unsafe { &*exec })
    }

    pub(crate) fn with_reactor<R>(f: impl for<'a> FnOnce(&'a mut DefaultReactor) -> R) -> R {
        Self::with(|exec| {
            let mut reactor = exec
                .reactor
                .try_borrow_mut()
                .expect("attempted nested reactor access");
            f(&mut reactor)
        })
    }

    /// Like [`Self::with_reactor`], but a no-op when the executor is gone or
    /// the reactor is busy — for drop paths that may run during teardown.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub(crate) fn try_with_reactor(f: impl for<'a> FnOnce(&'a mut DefaultReactor)) {
        let exec = EXEC.try_with(Cell::get).unwrap_or(std::ptr::null());
        let own = OWN
            .try_with(|o| o.0.try_borrow().ok().and_then(|o| o.clone()))
            .ok()
            .flatten();
        let exec = match own {
            Some(ref own) if exec.is_null() => Rc::as_ptr(own),
            _ => exec,
        };
        // SAFETY: as in `with`; `own` keeps the default executor alive.
        if let Some(exec) = unsafe { exec.as_ref() }
            && let Ok(mut reactor) = exec.reactor.try_borrow_mut()
        {
            f(&mut reactor);
        }
    }
}

pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: Send + 'static,
{
    JoinHandle(Executor::with(|exec| {
        let handle = exec.scheduler.spawn(f);
        exec.track(handle.task_ref());
        handle
    }))
}

pub fn sleep(duration: Duration) -> maitake::time::Sleep<'static> {
    let timer: &'static Timer = unsafe { &*Executor::with(|e| &e.timer as *const _) };
    timer.sleep(duration)
}

#[pin_project]
pub struct JoinHandle<O>(#[pin] pub maitake::task::JoinHandle<O>);

impl<O> JoinHandle<O> {
    pub fn drop_guard(self) -> JoinHandleDropGuard<O> {
        JoinHandleDropGuard(self)
    }

    /// Whether the task has finished, by completing or by being canceled.
    pub fn is_finished(&self) -> bool {
        self.0.is_complete()
    }
}

pub struct JoinHandleDropGuard<T>(crate::JoinHandle<T>);

impl<T> JoinHandleDropGuard<T> {
    /// Whether the task has finished, by completing or by being canceled.
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl<T> Drop for JoinHandleDropGuard<T> {
    fn drop(&mut self) {
        self.0.0.cancel();
    }
}

impl<O> Future for JoinHandle<O> {
    type Output = <maitake::task::JoinHandle<O> as Future>::Output;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().0.poll(cx)
    }
}

#[cfg(feature = "futures")]
impl<O> futures::future::FusedFuture for JoinHandle<O> {
    fn is_terminated(&self) -> bool {
        self.0.is_complete()
    }
}

#[macro_export]
macro_rules! rent {
    ($call:expr, $buf:ident) => {{
        let (res, o) = $call;
        $buf = o;
        res
    }};
}

#[macro_export]
macro_rules! rent_read {
    ($call:expr, $buf:ident) => {
        $crate::rent!($call, $buf).map(|len| &$buf[..len])
    };
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use maitake::sync::WaitCell;

    #[stellarator_macros::test]
    async fn test_sleep() {
        let start = Instant::now();
        println!("sleep start");
        crate::sleep(Duration::from_millis(250)).await;
        println!("slept");
        let delta = start.elapsed().as_millis().abs_diff(250);
        assert!(delta <= 10, "Δt ({}) > 10ms", delta)
    }

    /// Canceling a task parked in `sleep`, then outliving its timer deadline:
    /// the wheel entry must not fire a waker into the freed task (regression —
    /// the stale entry's wake underflowed the dead task's ref count).
    #[stellarator_macros::test]
    async fn test_cancel_while_sleeping() {
        let handle = crate::spawn(async {
            crate::sleep(Duration::from_millis(100)).await;
        })
        .drop_guard();
        crate::yield_now().await; // let the task park in the timer wheel
        drop(handle); // cancel it mid-sleep
        crate::sleep(Duration::from_millis(300)).await; // outlive its deadline
    }

    #[stellarator_macros::test]
    async fn test_cross_thread_wake() {
        let a = Arc::new(WaitCell::new());
        let b = a.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            b.wake();
            println!("woke wait cell");
        });

        println!("waiting wait cell");
        a.wait().await.unwrap();
    }
}
