//! One background thread running one executor for a group of async systems.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_lite::FutureExt;

use crate::async_system::{Stop, StopHandle, stop_pair};
use crate::coordinator::{BuildError, Launch, ParamError};

/// How long a dropped group waits for its thread before detaching it.
const JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// What a panicked task left behind, read by its system's adapter.
pub(crate) type Panicked = Arc<Mutex<Option<String>>>;

/// One async system on its way to a thread.
pub(crate) struct Member {
    pub id: String,
    pub launch: Box<dyn Launch>,
    pub panicked: Panicked,
}

/// A `GroupHandle` owns one thread; dropping it stops and joins that thread.
pub(crate) struct GroupHandle {
    name: String,
    stop: StopHandle,
    finished: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Starts `members` on one new thread, returning once every system is built.
pub(crate) fn spawn(name: &str, members: Vec<Member>) -> Result<Arc<GroupHandle>, BuildError> {
    let (handle, stop) = stop_pair();
    let finished = Arc::new(AtomicBool::new(false));
    let (report, built) = sync_channel(1);
    let thread = {
        let finished = finished.clone();
        std::thread::Builder::new()
            .name(format!("fsw-{name}"))
            .spawn(move || {
                stellarator::run(move || run(members, stop, report));
                finished.store(true, Ordering::Release);
            })
            .map_err(|_| BuildError::ThreadStart {
                thread: name.to_string(),
            })?
    };
    let group = Arc::new(GroupHandle {
        name: name.to_string(),
        stop: handle,
        finished,
        thread: Some(thread),
    });
    match collect(built, name)? {
        Some((id, source)) => Err(BuildError::Params { id, source }),
        None => Ok(group),
    }
}

/// Waits for the thread's build report, taking a closed channel as a panic.
fn collect(
    built: Receiver<Option<(String, ParamError)>>,
    name: &str,
) -> Result<Option<(String, ParamError)>, BuildError> {
    built.recv().map_err(|_| BuildError::ThreadStart {
        thread: name.to_string(),
    })
}

/// Builds every member on this thread, then runs them until they end.
async fn run(members: Vec<Member>, stop: Stop, report: SyncSender<Option<(String, ParamError)>>) {
    let mut tasks = Vec::with_capacity(members.len());
    for member in members {
        match member.launch.launch(stop.clone()) {
            Ok(running) => tasks.push(stellarator::spawn(task(running, member.panicked))),
            Err(source) => {
                let _ = report.send(Some((member.id, source)));
                return;
            }
        }
    }
    let _ = report.send(None);
    for task in tasks {
        let _ = task.await;
    }
}

/// Runs one system, recording a panic for its adapter instead of unwinding out.
async fn task(running: crate::async_system::Running, panicked: Panicked) {
    let caught = std::panic::AssertUnwindSafe(running).catch_unwind().await;
    if let Err(payload) = caught {
        let message = crate::coordinator::panic_message(&*payload).to_string();
        crate::panic::discard(payload);
        // PANIC Safety: the adapter only replaces this slot, never panicking
        // while it holds the lock.
        *panicked.lock().expect("an unpoisoned slot") = Some(message);
    }
}

impl Drop for GroupHandle {
    /// Stops the thread and joins it, detaching it if it outlives the timeout.
    fn drop(&mut self) {
        self.stop.stop();
        let deadline = Instant::now() + JOIN_TIMEOUT;
        while !self.finished.load(Ordering::Acquire) {
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
    use std::sync::mpsc::channel;

    use metor_fsw_3_ring::{Config, Notifier, RingBuffer};

    use crate::port::{Input, Output, ring_capacity};
    use crate::record::Record;
    use crate::tests::utils::Imu;

    /// The wake this slice rests on: a ring write on this thread must wake a
    /// task parked on an executor of its own.
    #[test]
    fn a_write_here_wakes_a_task_on_a_spawned_executor() {
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
