//! One background thread running one executor for a group of async systems.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_lite::FutureExt;
use stellarator::sync::WaitQueue;

use crate::async_system::{Stop, StopHandle, stop_pair};
use crate::coordinator::{BuildError, ParamError};

use super::Launch;

/// How long a dropped group waits for its thread before detaching it.
const JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// What a panicked task left behind, read by its system's adapter.
///
/// The flag is what the adapter reads each cycle; the message is taken once.
#[derive(Clone, Default)]
pub(crate) struct Panicked(Arc<Slot>);

#[derive(Default)]
struct Slot {
    set: AtomicBool,
    message: Mutex<Option<String>>,
}

impl Panicked {
    fn set(&self, message: String) {
        // PANIC Safety: neither side panics while it holds this lock.
        *self.0.message.lock().expect("an unpoisoned slot") = Some(message);
        self.0.set.store(true, Ordering::Release);
    }

    /// The message the task left, once.
    pub(crate) fn take(&self) -> Option<String> {
        if !self.0.set.load(Ordering::Acquire) {
            return None;
        }
        // PANIC Safety: as above.
        self.0.message.lock().expect("an unpoisoned slot").take()
    }
}

/// One async system on its way to a thread.
pub(crate) struct Member {
    pub launch: Box<dyn Launch>,
    pub panicked: Panicked,
}

/// One member and the slot its construction result is reported through.
struct Job {
    member: Member,
    report: SyncSender<Option<ParamError>>,
}

/// The jobs waiting for the group's thread to build them.
struct Inbox {
    jobs: Mutex<Vec<Job>>,
    woken: WaitQueue,
}

impl Inbox {
    /// Takes every waiting job.
    fn take(&self) -> Vec<Job> {
        // PANIC Safety: neither side panics while it holds this lock.
        core::mem::take(&mut *self.jobs.lock().expect("an unpoisoned inbox"))
    }

    fn is_empty(&self) -> bool {
        // PANIC Safety: as above.
        self.jobs.lock().expect("an unpoisoned inbox").is_empty()
    }
}

/// A `GroupHandle` owns one thread; dropping it stops and joins that thread.
pub(crate) struct GroupHandle {
    name: String,
    stop: StopHandle,
    inbox: Arc<Inbox>,
    thread: Option<JoinHandle<()>>,
}

/// Starts one empty group on a new thread, which takes members as they come.
pub(crate) fn spawn(name: &str) -> Result<Arc<GroupHandle>, BuildError> {
    let (handle, stop) = stop_pair();
    let inbox = Arc::new(Inbox {
        jobs: Mutex::new(Vec::new()),
        woken: WaitQueue::new(),
    });
    let thread = {
        let inbox = inbox.clone();
        std::thread::Builder::new()
            .name(format!("fsw-{name}"))
            .spawn(move || stellarator::run(move || run(inbox, stop)))
            .map_err(|_| BuildError::ThreadStart {
                thread: name.to_string(),
            })?
    };
    Ok(Arc::new(GroupHandle {
        name: name.to_string(),
        stop: handle,
        inbox,
        thread: Some(thread),
    }))
}

impl GroupHandle {
    /// Builds one member on this group's thread, returning once it is running.
    pub(crate) fn add(&self, member: Member) -> Result<(), ParamError> {
        let (report, built) = sync_channel(1);
        // PANIC Safety: neither side panics while it holds this lock.
        self.inbox
            .jobs
            .lock()
            .expect("an unpoisoned inbox")
            .push(Job { member, report });
        self.inbox.woken.wake_all();
        match built.recv() {
            Ok(None) => Ok(()),
            Ok(Some(source)) => Err(source),
            Err(_) => Err(ParamError::Decode(format!(
                "thread `{}` stopped before it built the system",
                self.name
            ))),
        }
    }
}

/// Builds every member the group is handed, running them until the stop is set.
async fn run(inbox: Arc<Inbox>, stop: Stop) {
    let mut tasks = Vec::new();
    while !stop.is_set() {
        for Job { member, report } in inbox.take() {
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
        let _ = inbox
            .woken
            .wait_for(|| stop.is_set() || !inbox.is_empty())
            .await;
    }
    for task in tasks {
        let _ = task.await;
    }
}

/// Constructs one system, a panicking constructor reading as a param error
/// rather than taking the group's thread down with it.
fn build(launch: Box<dyn Launch>, stop: Stop) -> Result<crate::async_system::Running, ParamError> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| launch.launch(stop))) {
        Ok(result) => result,
        Err(payload) => {
            let message = crate::coordinator::panic_message(&*payload).to_string();
            crate::panic::discard(payload);
            Err(ParamError::Decode(message))
        }
    }
}

/// Runs one system, recording a panic for its adapter instead of unwinding out.
async fn task(running: crate::async_system::Running, panicked: Panicked) {
    let caught = std::panic::AssertUnwindSafe(running).catch_unwind().await;
    if let Err(payload) = caught {
        let message = crate::coordinator::panic_message(&*payload).to_string();
        crate::panic::discard(payload);
        panicked.set(message);
    }
}

impl Drop for GroupHandle {
    /// Stops the thread and joins it, detaching it if it outlives the timeout.
    fn drop(&mut self) {
        self.stop.stop();
        self.inbox.woken.wake_all();
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
    use std::sync::mpsc::{channel, sync_channel};

    use metor_fsw_3_ring::{Config, Notifier, RingBuffer};

    use super::*;
    use crate::async_system::Running;
    use crate::port::{Input, Output, ring_capacity};
    use crate::record::Record;
    use crate::tests::utils::Imu;

    /// A launch that panics where a user's constructor would.
    struct PanicCtor;

    impl Launch for PanicCtor {
        fn launch(self: Box<Self>, _stop: Stop) -> Result<Running, ParamError> {
            panic!("a constructor that panics")
        }
    }

    /// A launch whose task says it started, then parks.
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
        let Err(ParamError::Decode(message)) = group.add(member(Box::new(PanicCtor))) else {
            panic!("a panicking constructor is a param error")
        };
        assert!(message.contains("a constructor that panics"), "{message}");

        let (tx, started) = sync_channel(1);
        group
            .add(member(Box::new(Started(tx))))
            .expect("the group still builds");
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok(()));

        // A thread that died would be detached at the timeout instead.
        let at = Instant::now();
        drop(group);
        assert!(at.elapsed() < JOIN_TIMEOUT / 2, "{:?}", at.elapsed());
    }

    /// The wake this slice rests on: a ring write on this thread must wake a
    /// task parked on an executor of its own.
    #[test]
    fn test_write_wakes_executor() {
        let wake = Notifier::default();
        let ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(Imu::MAX_LEN, 4).expect("valid capacity"),
            max_readers: 1,
        });
        let view = ring.view(wake.clone()).expect("free slot");
        let (tx, rx) = channel();
        let thread = std::thread::spawn(move || {
            stellarator::run(move || async move {
                let mut input = Input::<Imu, Notifier>::try_new(vec![view]).expect("aligned");
                let sample = input.next().await.expect("record").sample;
                tx.send(sample).expect("the test is listening");
            })
        });
        std::thread::sleep(core::time::Duration::from_millis(50));
        let mut out =
            Output::<Imu, _>::try_new(ring.writer(wake).expect("free writer")).expect("aligned");
        out.write(&Imu::new(1, 42.0)).expect("ring has room");
        assert_eq!(
            rx.recv_timeout(core::time::Duration::from_secs(5)),
            Ok(42.0)
        );
        thread.join().expect("the executor thread finished");
    }
}
