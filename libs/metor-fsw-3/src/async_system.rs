//! Systems written as one long-running `async fn` on a background thread.

use core::future::Future;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use maitake_sync::WaitQueue;
use metor_fsw_3_ring::Notifier;

use crate::def::{DefCx, DefError};
use crate::system::{SystemDef, SystemInputs, SystemOutputs};

/// A system that runs on its own thread and sets its own pace.
///
/// Unlike a cyclic system, [`AsyncSystem::run`] is called once and keeps
/// running until `stop` is set. Use it for work that blocks or doesn't fit in
/// a cycle, like network IO. Its ports work like a cyclic system's, but the
/// thread adapter copies their data to and from the schedule. Waiting on an
/// input parks the background thread, not the schedule.
#[allow(async_fn_in_trait)]
pub trait AsyncSystem {
    type State;
    type Inputs: SystemInputs<Notifier>;
    type Outputs: SystemOutputs;

    /// Returns this instance's ports, computed from its config.
    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError>;

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

/// Tells an async system when to stop.
///
/// Every system placed on the same thread shares one stop.
#[derive(Clone)]
pub struct Stop(Arc<StopInner>);

/// Sets a [`Stop`], telling every system that shares it to return.
#[derive(Clone)]
pub struct StopHandle(Arc<StopInner>);

/// Creates a linked [`StopHandle`] and [`Stop`].
///
/// A thread group keeps the handle and gives the stop to its systems.
pub fn stop_pair() -> (StopHandle, Stop) {
    let inner = Arc::new(StopInner {
        set: AtomicBool::new(false),
        woken: WaitQueue::new(),
    });
    (StopHandle(inner.clone()), Stop(inner))
}

impl Stop {
    /// Returns whether the stop has been set.
    pub fn is_set(&self) -> bool {
        self.0.set.load(Ordering::Acquire)
    }

    /// Waits until the stop is set.
    ///
    /// The stop may be set from another thread.
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
}

/// A launched async system, as a future.
///
/// Polling it to completion runs the system until it stops.
pub type Running = core::pin::Pin<Box<dyn Future<Output = ()>>>;

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;

    #[stellarator::test]
    async fn test_stop_resolves() {
        let (handle, stop) = stop_pair();
        assert!(!stop.is_set());
        handle.stop();
        assert!(stop.is_set());
        stop.wait().await;
    }

    #[stellarator::test]
    async fn test_stop_wakes_across_threads() {
        let (handle, stop) = stop_pair();
        std::thread::spawn(move || {
            std::thread::sleep(core::time::Duration::from_millis(20));
            handle.stop();
        });
        stop.wait().await;
        assert!(stop.is_set());
    }
}
