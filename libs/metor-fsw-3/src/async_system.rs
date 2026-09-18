//! Systems authored as one `async fn run` on a background thread.

use core::future::Future;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use metor_fsw_3_ring::Notifier;
use stellarator::sync::WaitQueue;

use crate::system::{SystemDef, SystemInputs, SystemOutputs};

/// An `AsyncSystem` runs once, paces itself, and returns when `stop` resolves.
///
/// Its ports are the same as a cyclic system's, over the mirror rings the
/// thread adapter copies across, so a read parks the background thread.
#[allow(async_fn_in_trait)]
pub trait AsyncSystem {
    type State;
    type Inputs: SystemInputs<Notifier>;
    type Outputs: SystemOutputs;

    fn def() -> SystemDef;

    async fn run(
        &self,
        state: &mut Self::State,
        inputs: &mut Self::Inputs,
        outputs: &mut Self::Outputs,
        stop: Stop,
    );
}

struct StopInner {
    set: AtomicBool,
    woken: WaitQueue,
}

/// A `Stop` is the end of an async system's `run`, shared with its group.
#[derive(Clone)]
pub struct Stop(Arc<StopInner>);

/// A `StopHandle` ends every [`Stop`] it was made with.
#[derive(Clone)]
pub struct StopHandle(Arc<StopInner>);

/// Returns the handle the adapter keeps and the stop its systems await.
pub fn stop_pair() -> (StopHandle, Stop) {
    let inner = Arc::new(StopInner {
        set: AtomicBool::new(false),
        woken: WaitQueue::new(),
    });
    (StopHandle(inner.clone()), Stop(inner))
}

impl Stop {
    /// Whether the stop has been set.
    pub fn is_set(&self) -> bool {
        self.0.set.load(Ordering::Acquire)
    }

    /// Resolves once the stop is set, including from another thread.
    pub async fn wait(&self) {
        let inner = &self.0;
        let _ = inner
            .woken
            .wait_for(|| inner.set.load(Ordering::Acquire))
            .await;
    }
}

impl StopHandle {
    /// Sets the stop and wakes every task waiting on it.
    pub fn stop(&self) {
        self.0.set.store(true, Ordering::Release);
        self.0.woken.wake_all();
    }

    /// Whether the stop has been set.
    pub fn is_set(&self) -> bool {
        self.0.set.load(Ordering::Acquire)
    }
}

/// The future an async system's launch produces, owning its state and ports.
pub type Running = core::pin::Pin<Box<dyn Future<Output = ()>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_stop_is_unset() {
        let (handle, stop) = stop_pair();
        assert!(!stop.is_set());
        assert!(!handle.is_set());
    }

    #[stellarator::test]
    async fn stop_resolves_once_set() {
        let (handle, stop) = stop_pair();
        handle.stop();
        assert!(stop.is_set());
        stop.wait().await;
    }

    #[stellarator::test]
    async fn a_stop_set_from_another_thread_wakes_the_waiter() {
        let (handle, stop) = stop_pair();
        std::thread::spawn(move || {
            std::thread::sleep(core::time::Duration::from_millis(20));
            handle.stop();
        });
        stop.wait().await;
        assert!(stop.is_set());
    }
}
