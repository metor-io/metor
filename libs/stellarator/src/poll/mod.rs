use maitake::time::Timer;
use pin_project::{pin_project, pinned_drop};
use polling::Poller;
use slab::Slab;
use smallvec::SmallVec;
use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Poll, Waker},
    time::Duration,
};

#[cfg(not(target_os = "windows"))]
use std::os::fd::RawFd;
#[cfg(target_os = "windows")]
use std::os::windows::io::RawHandle as RawFd;

pub mod ops;

use crate::{Error, Executor, Reactor};

pub struct PollingReactor {
    poller: Arc<Poller>,
    states: Slab<OpState>,
    /// The live ops waiting on each registered fd, with the interest each
    /// one asked for. The fd is registered with the poller for the *union*
    /// of these interests: `polling`'s `modify` replaces an fd's interest
    /// set wholesale, so registering only the newest op's interest would
    /// silently disarm every other op on the fd (a parked write on a split
    /// socket would never wake once the read half re-registered).
    fds: HashMap<RawFd, SmallVec<[(CompletionId, polling::Event); 4]>>,
    events: polling::Events,
}

#[cfg(not(target_os = "windows"))]
fn borrow_source(fd: RawFd) -> std::os::fd::BorrowedFd<'static> {
    unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
}
#[cfg(target_os = "windows")]
fn borrow_source(fd: RawFd) -> std::os::windows::io::BorrowedSocket<'static> {
    unsafe { std::os::windows::io::BorrowedSocket::borrow_raw(fd as _) }
}

pub enum OpState {
    Waiting(Option<Waker>),
    Ready,
}

impl Reactor for PollingReactor {
    fn wait_for_io(&mut self, timeout: Option<Duration>) -> Result<(), crate::Error> {
        self.poller.wait(&mut self.events, timeout)?;
        Ok(())
    }

    fn process_io(&mut self) -> Result<(), crate::Error> {
        if self.events.is_empty() {
            self.poller.wait(&mut self.events, Some(Duration::ZERO))?;
        }
        for event in self.events.iter() {
            let Some(ops) = self.fds.get(&(event.key as RawFd)) else {
                continue;
            };
            for (id, _) in ops.iter() {
                let Some(state) = self.states.get_mut(id.0) else {
                    continue;
                };
                if let OpState::Waiting(Some(waker)) = state {
                    waker.wake_by_ref();
                }
                *state = OpState::Ready;
            }
        }
        self.events.clear();
        Ok(())
    }

    type RemainingIo = ();
    fn drain_io(&mut self) -> Self::RemainingIo {}

    fn waker(&self) -> Waker {
        let poller = self.poller.clone();
        waker_fn::waker_fn(move || {
            for _ in 0..1024 {
                if poller.notify().is_ok() {
                    break;
                }
            }
        })
    }

    fn external_waker(&self) -> impl maitake::scheduler::ExternalWaker {
        ExternalWaker {
            poller: self.poller.clone(),
        }
    }
}
#[derive(Debug)]
struct ExternalWaker {
    poller: Arc<Poller>,
}

impl maitake::scheduler::ExternalWaker for ExternalWaker {
    fn wake(&self) {
        for _ in 0..1024 {
            if self.poller.notify().is_ok() {
                break;
            }
        }
    }
}

impl PollingReactor {
    pub fn submit_op<O: OpCode>(&mut self, op_code: O) -> Result<Completion<O>, Error> {
        let id = CompletionId(self.states.insert(op_code.initial_state()));
        self.submit_op_reactor(&op_code, id)?;
        Ok(Completion { id, op_code })
    }

    fn submit_op_reactor(&mut self, op_code: &impl OpCode, id: CompletionId) -> Result<(), Error> {
        if let Some(event) = op_code.event() {
            let fd = event.key as RawFd;
            let source = borrow_source(fd);
            if let Some(ops) = self.fds.get_mut(&fd) {
                // A pending op re-submits after every `Poll::Pending`;
                // update its slot rather than growing the list.
                match ops.iter_mut().find(|(i, _)| i.0 == id.0) {
                    Some((_, interest)) => *interest = event,
                    None => ops.push((id, event)),
                }
                let union = ops
                    .iter()
                    .fold(polling::Event::none(event.key), |acc, (_, e)| {
                        polling::Event::new(
                            acc.key,
                            acc.readable || e.readable,
                            acc.writable || e.writable,
                        )
                    });
                self.poller.modify(source, union)?;
            } else {
                unsafe { self.poller.add(&source, event)? };
                self.fds.insert(fd, smallvec::smallvec![(id, event)]);
            }
        }
        Ok(())
    }

    /// Forget a finished (or dropped) op: drop its interest from the fd's
    /// union and unregister the fd once nothing waits on it, so a later op
    /// on a reused fd number starts from a clean `add`.
    fn release_op(&mut self, event: Option<polling::Event>, id: CompletionId) {
        let _ = self.states.try_remove(id.0);
        let Some(event) = event else { return };
        let fd = event.key as RawFd;
        let Some(ops) = self.fds.get_mut(&fd) else {
            return;
        };
        ops.retain(|(i, _)| i.0 != id.0);
        if ops.is_empty() {
            self.fds.remove(&fd);
            // The op borrowed the fd, so it is still open here; a failed
            // delete just means the registration is already gone.
            let _ = self.poller.delete(borrow_source(fd));
        }
    }

    pub fn poll_completion<O: OpCode>(
        &mut self,
        cx: &mut std::task::Context<'_>,
        completion: Pin<&mut Completion<O>>,
    ) -> Poll<O::Output> {
        let mut completion = completion.project();
        let state = self
            .states
            .get_mut(completion.id.0)
            .expect("state not found for completion");

        match state {
            OpState::Waiting(Some(waker)) if waker.will_wake(cx.waker()) => {
                // nothing to do here
            }
            OpState::Waiting(_) => {
                // replace waker with this contexts waker if it is `None` or the current waker won't wake this context
                *state = OpState::Waiting(Some(cx.waker().clone()));
            }
            OpState::Ready => {}
        }
        match completion.op_code.as_mut().poll(cx) {
            Poll::Pending => {
                let op_code = completion.op_code.as_ref();
                *state = OpState::Waiting(Some(cx.waker().clone()));
                self.submit_op_reactor(op_code.get_ref(), *completion.id)
                    .expect("failed to submit op to reactor");
                Poll::Pending
            }
            Poll::Ready(res) => {
                self.release_op(completion.op_code.event(), *completion.id);
                Poll::Ready(res)
            }
        }
    }
}

pub trait OpCode {
    fn initial_state(&self) -> OpState {
        OpState::Ready
    }

    fn event(&self) -> Option<polling::Event> {
        None
    }

    type Output;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output>;
}
#[derive(Clone, Copy)]
pub struct CompletionId(pub usize);

#[pin_project(PinnedDrop, project = CompletionProj)]
pub struct Completion<O: OpCode> {
    id: CompletionId,
    #[pin]
    op_code: O,
}

impl<O: OpCode> Completion<O> {
    pub fn submit(op_code: O) -> Result<Self, Error> {
        Executor::with_reactor(|reactor| reactor.submit_op(op_code))
    }

    pub async fn run(op_code: O) -> O::Output {
        Self::submit(op_code).expect("failed to submit op").await
    }
}

impl<O: OpCode> Future for Completion<O> {
    type Output = O::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        Executor::with_reactor(|r| r.poll_completion(cx, self))
    }
}

#[pinned_drop]
impl<O: OpCode> PinnedDrop for Completion<O> {
    fn drop(self: Pin<&mut Self>) {
        // Tolerant of executor teardown: cancelled tasks may drop their
        // completions while the executor itself is going away.
        Executor::try_with_reactor(|r| r.release_op(self.op_code.event(), self.id));
    }
}

impl Default for Executor<PollingReactor> {
    fn default() -> Self {
        Self::try_new().unwrap()
    }
}

impl Executor<PollingReactor> {
    pub fn try_new() -> Result<Self, Error> {
        let reactor = PollingReactor {
            poller: Arc::new(Poller::new()?),
            states: Slab::with_capacity(256),
            events: Default::default(),
            fds: Default::default(),
        };
        let scheduler =
            maitake::scheduler::LocalScheduler::with_external_waker(reactor.external_waker());
        Ok(Executor {
            reactor: RefCell::new(reactor),
            scheduler,
            timer: Timer::new(crate::os::os_clock()),
        })
    }
}
