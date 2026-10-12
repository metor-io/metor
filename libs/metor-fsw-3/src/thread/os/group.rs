//! A thread group: one background thread running an executor shared by
//! several async systems.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_lite::FutureExt;
use thingbuf::mpsc::{Receiver, Sender, with_recycle};
use thingbuf::recycling::Recycle;

use crate::async_system::{Stop, StopHandle, stop_pair};
use crate::coordinator::ParamError;

use super::Launch;

/// How long a dropped group waits for its thread to stop before giving up
/// and detaching it.
const JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Reports a task's panic to its system's adapter.
///
/// The adapter checks it every cycle. The check is a cheap atomic load, and
/// the panic message can be taken only once.
#[derive(Clone, Default)]
pub(crate) struct Panicked(Arc<Slot>);

#[derive(Default)]
struct Slot {
    set: AtomicBool,
    message: Mutex<Option<String>>,
}

impl Panicked {
    fn set(&self, message: String) {
        // PANIC Safety: no code panics while holding this lock.
        *self.0.message.lock().expect("an unpoisoned slot") = Some(message);
        self.0.set.store(true, Ordering::Release);
    }

    /// Takes the panic message, if the task panicked. Later calls return
    /// `None`.
    pub(crate) fn take(&self) -> Option<String> {
        if !self.0.set.load(Ordering::Acquire) {
            return None;
        }
        // PANIC Safety: as above.
        self.0.message.lock().expect("an unpoisoned slot").take()
    }
}

/// An async system waiting to be built on a group's thread.
pub(crate) struct Member {
    pub launch: Box<dyn Launch>,
    pub panicked: Panicked,
}

/// A member queued for the group's thread, and the channel that reports
/// whether building it succeeded.
struct Job {
    member: Member,
    report: SyncSender<Option<ParamError>>,
}

/// How many jobs the inbox holds. `add` waits for each job's report, so a
/// caller has at most one in flight.
const INBOX_CAPACITY: usize = 4;

/// One inbox slot. The `Mutex` makes it `Sync`, which the queue needs; it is
/// never locked, since a job is only moved in and out.
type InboxSlot = Option<Mutex<Job>>;

/// Leaves an inbox slot empty once its job is taken.
struct EmptySlot;

impl Recycle<InboxSlot> for EmptySlot {
    fn new_element(&self) -> InboxSlot {
        None
    }

    fn recycle(&self, job: &mut InboxSlot) {
        *job = None;
    }
}

/// A handle to a running thread group. Dropping it stops and joins the thread.
pub(crate) struct GroupHandle {
    name: String,
    stop: StopHandle,
    inbox: Sender<InboxSlot, EmptySlot>,
    thread: Option<JoinHandle<()>>,
}

/// Starts an empty group on a new thread. Members are added with
/// [`GroupHandle::add`].
pub(crate) fn spawn(name: &str) -> Result<Arc<GroupHandle>, ParamError> {
    let (handle, stop) = stop_pair();
    let (inbox, jobs) = with_recycle(INBOX_CAPACITY, EmptySlot);
    let thread = std::thread::Builder::new()
        .name(format!("fsw-{name}"))
        .spawn(move || stellarator::run(move || run(jobs, stop)))
        .map_err(|_| ParamError::ThreadStart {
            thread: name.to_string(),
        })?;
    Ok(Arc::new(GroupHandle {
        name: name.to_string(),
        stop: handle,
        inbox,
        thread: Some(thread),
    }))
}

impl GroupHandle {
    /// Builds `member` on this group's thread. Returns once it is running, or
    /// with the error that stopped it from building.
    pub(crate) fn add(&self, member: Member) -> Result<(), ParamError> {
        let (report, built) = sync_channel(1);
        let queued = self
            .inbox
            .try_send(Some(Mutex::new(Job { member, report })));
        let thread = || ParamError::ThreadStart {
            thread: self.name.clone(),
        };
        queued.map_err(|_| thread())?;
        match built.recv() {
            Ok(None) => Ok(()),
            Ok(Some(source)) => Err(source),
            Err(_) => Err(thread()),
        }
    }
}

/// The group thread's main loop. Builds and runs each member it is handed,
/// until `stop` is set.
async fn run(jobs: Receiver<InboxSlot, EmptySlot>, stop: Stop) {
    let mut tasks = Vec::new();
    while let Some(job) = stopped(&stop).or(jobs.recv()).await {
        let Some(job) = job else { continue };
        let Job { member, report } = job.into_inner().unwrap_or_else(PoisonError::into_inner);
        let Member { launch, panicked } = member;
        match build(launch, stop.clone()) {
            Ok(running) => {
                tasks.push(stellarator::spawn(task(running, panicked)));
                let _ = report.send(None);
            }
            Err(source) => {
                let _ = report.send(Some(source));
            }
        }
    }
    for task in tasks {
        let _ = task.await;
    }
}

/// Resolves to `None` once `stop` is set.
async fn stopped(stop: &Stop) -> Option<InboxSlot> {
    stop.wait().await;
    None
}

/// Builds one system.
///
/// A panic in the system's constructor becomes a `ParamError`, so it does not
/// take down the group's thread.
pub(super) fn build(
    launch: Box<dyn Launch>,
    stop: Stop,
) -> Result<crate::async_system::Running, ParamError> {
    std::panic::catch_unwind(AssertUnwindSafe(|| launch.launch(stop))).map_err(|payload| {
        let message = crate::panic::message(&*payload).to_string();
        crate::panic::discard(payload);
        ParamError::Panicked(message)
    })?
}

/// Runs one system. If it panics, the panic is caught and reported to its
/// adapter.
pub(super) async fn task(running: crate::async_system::Running, panicked: Panicked) {
    let caught = std::panic::AssertUnwindSafe(running).catch_unwind().await;
    if let Err(payload) = caught {
        let message = crate::panic::message(&*payload).to_string();
        crate::panic::discard(payload);
        panicked.set(message);
    }
}

impl Drop for GroupHandle {
    /// Stops the thread and joins it. If the thread is still running after
    /// [`JOIN_TIMEOUT`], it is detached instead.
    fn drop(&mut self) {
        self.stop.stop();
        let deadline = Instant::now() + JOIN_TIMEOUT;
        while self
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
        {
            if Instant::now() >= deadline {
                tracing::error!(thread = self.name, "detaching a thread that would not stop");
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use super::*;
    use crate::async_system::Running;

    /// A launch that panics, like a user's constructor might.
    struct PanicCtor;

    impl Launch for PanicCtor {
        fn launch(self: Box<Self>, _stop: Stop) -> Result<Running, ParamError> {
            panic!("a constructor that panics")
        }
    }

    /// A launch whose task signals that it started, then waits forever.
    struct Started(SyncSender<()>);

    impl Launch for Started {
        fn launch(self: Box<Self>, stop: Stop) -> Result<Running, ParamError> {
            let started = self.0;
            Ok(Box::pin(async move {
                let _ = started.send(());
                stop.wait().await;
            }))
        }
    }

    fn member(launch: Box<dyn Launch>) -> Member {
        Member {
            launch,
            panicked: Panicked::default(),
        }
    }

    #[test]
    fn test_group_survives_constructor_panic() {
        let group = spawn("ctor").expect("a thread");
        let Err(ParamError::Panicked(message)) = group.add(member(Box::new(PanicCtor))) else {
            panic!("a panicking constructor is a param error")
        };
        assert!(message.contains("a constructor that panics"), "{message}");

        let (tx, started) = sync_channel(1);
        group
            .add(member(Box::new(Started(tx))))
            .expect("the group still builds");
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok(()));

        // If the thread had died, the drop would wait for the timeout and
        // detach it instead.
        let at = Instant::now();
        drop(group);
        assert!(at.elapsed() < JOIN_TIMEOUT / 2, "{:?}", at.elapsed());
    }
}
