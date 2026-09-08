//! Tracks database producers through executor teardown, including disconnected
//! connections whose cancellation is still in flight.

use std::future::Future;
use std::sync::{Arc, Mutex};

use gpui::Global;
use stellarator::struc_con::{Joinable, Thread, ThreadBuilder};
use stellarator::util::CancelToken;

#[derive(Clone, Default)]
pub(crate) struct BackgroundTasks(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    stopped: bool,
    threads: Vec<(CancelToken, Thread<Option<()>>)>,
}

impl Global for BackgroundTasks {}

impl BackgroundTasks {
    pub(crate) fn spawn<F, Fut>(&self, cancel: CancelToken, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let mut state = self.0.lock().unwrap();
        if state.stopped {
            cancel.cancel();
            return;
        }
        let thread = ThreadBuilder::default()
            .cancel_token(cancel.clone())
            .stellar(f);
        state.threads.push((cancel, thread));
    }

    /// Called synchronously during quit: filesystem finalization must outlive
    /// GPUI's short deadline for asynchronous quit observers.
    pub(crate) fn shutdown(&self) -> Result<(), stellarator::Error> {
        let threads = {
            let mut state = self.0.lock().unwrap();
            state.stopped = true;
            for (cancel, _) in &state.threads {
                cancel.cancel();
            }
            std::mem::take(&mut state.threads)
        };
        let mut result = Ok(());
        for (_, thread) in threads {
            if let Err(err) = futures_lite::future::block_on(thread.join()) {
                result = Err(err);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn shutdown_waits_for_executor_teardown_and_rejects_new_work() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let tasks = BackgroundTasks::default();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        tasks.spawn(CancelToken::new(), move || async move {
            stellarator::spawn(async move {
                let _guard = guard;
                tx.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            std::future::pending::<()>().await;
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        tasks.shutdown().unwrap();
        assert!(dropped.load(Ordering::SeqCst));
        let cancel = CancelToken::new();
        tasks.spawn(cancel.clone(), || async {
            panic!("spawned after shutdown")
        });
        assert!(cancel.is_cancelled());
        tasks.shutdown().unwrap();
    }
}
