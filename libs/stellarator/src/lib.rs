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
    /// The executor `spawn`, `sleep`, and IO use on this thread. Whoever
    /// installs it keeps it alive until it is swapped back out.
    static EXEC: Cell<*const Executor> = const { Cell::new(std::ptr::null()) };
    /// The thread's own executor, made on first use and dropped by `run`.
    static OWN: RefCell<Option<Rc<Executor>>> = const { RefCell::new(None) };
}

/// Installs an executor in `EXEC` and restores the previous one on drop.
struct Installed(*const Executor);

impl Installed {
    /// Every `Executor` has the default reactor: its fields are private and
    /// only the default reactor's constructors build one.
    fn new<R: Reactor>(exec: &Executor<R>) -> Self {
        let exec = (exec as *const Executor<R>).cast::<Executor>();
        Self(EXEC.try_with(|e| e.replace(exec)).unwrap_or(exec))
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        let _ = EXEC.try_with(|e| e.set(self.0));
    }
}

/// What one [`Executor::turn`] left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Turned {
    /// A task is ready to run on the next turn.
    pub runnable: bool,
    /// How long until the timer's next deadline, by the executor's clock.
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

pub struct Executor<R: Reactor = DefaultReactor> {
    reactor: RefCell<R>,
    scheduler: maitake::scheduler::LocalScheduler,
    tasks: RefCell<Vec<TaskRef>>,
    timer: maitake::time::Timer,
}

impl<R: Reactor> Executor<R> {
    pub fn run<O, F>(&self, func: impl FnOnce() -> F) -> Result<O, Error>
    where
        F: Future<Output = O> + 'static,
        O: 'static,
    {
        let _installed = Installed::new(self);
        let future = func();
        let main_task = self.scheduler.spawn(future);
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
        self.reactor.borrow_mut().drain_io().cancel();
        Ok(out)
    }

    /// Records a task so `Drop` can cancel it, forgetting finished ones.
    fn track(&self, task: TaskRef) {
        let mut tasks = self.tasks.borrow_mut();
        tasks.retain(|t| !t.is_complete());
        tasks.push(task);
    }
}

impl<R: Reactor> Drop for Executor<R> {
    /// Cancels every task and ticks once, so each drops its future, and any
    /// `Sleep` on this timer, before the timer drops. Then cancels the IO.
    fn drop(&mut self) {
        let this = &*self;
        let _installed = Installed::new(this);
        for task in this.tasks.borrow().iter() {
            task.cancel();
        }
        this.scheduler.tick();
        this.scheduler.cancel_all();
        this.tasks.borrow_mut().clear();
        this.reactor.borrow_mut().drain_io().cancel();
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

/// Runs `func`'s future to completion on this thread's own executor, then
/// drops that executor. The next `run` makes a new one.
pub fn run<R, F>(func: impl FnOnce() -> F) -> R
where
    F: Future<Output = R> + 'static,
    R: 'static,
{
    let exec = Executor::own();
    let out = exec.run(func).unwrap();
    OWN.with(|o| o.borrow_mut().take());
    out
}

impl Executor<DefaultReactor> {
    /// An executor with its own reactor and the given clock.
    pub fn new(clock: time::Clock) -> Result<Self, Error> {
        let mut exec = Self::try_new()?;
        exec.timer = Timer::new(clock.maitake());
        Ok(exec)
    }

    /// One pass: timer, IO, one scheduler tick. Never blocks and never moves
    /// the clock. Installs `self` as this thread's executor for the pass.
    pub fn turn(self: &Rc<Self>) -> Turned {
        let _installed = Installed::new(&**self);
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

    /// Spawns onto this executor, installed as for [`Executor::turn`].
    ///
    /// Every task and `JoinHandle` spawned here must drop before the executor.
    pub fn spawn<F>(self: &Rc<Self>, f: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: Send + 'static,
    {
        let _installed = Installed::new(&**self);
        spawn(f)
    }

    /// This thread's own executor, made if there is none.
    fn own() -> Rc<Self> {
        OWN.with(|o| {
            o.borrow_mut()
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
        // SAFETY: an installed executor is kept alive by whoever installed
        // it, and `own` keeps the thread's own executor alive for the call.
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
        // SAFETY: as in `with`.
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
}

pub struct JoinHandleDropGuard<T>(crate::JoinHandle<T>);

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
