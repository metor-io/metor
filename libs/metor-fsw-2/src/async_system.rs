//! The free-running authoring form, and the cancellation handle it waits on.
//!
//! Every other system form lives in `metor-fsw-2-core`, because a pack, a
//! shared library, or a wasm guest can construct one. An [`AsyncSystem`]
//! cannot: it owns a task the coordinator's executor spawns once and never
//! ticks, so it exists only where there is a runtime to own it. That is why it
//! is here rather than beside [`CyclicSystem`](crate::CyclicSystem).

use core::future::Future;

use metor_fsw_2_core::{System, SystemDescriptor, SystemKind, descriptor_for};

/// Cancellation-aware waits for an [`AsyncSystem`]'s run loop.
pub struct AsyncContext {
    pub(crate) cancel: stellarator::util::CancelToken,
}

impl AsyncContext {
    /// Runs `future` until it completes or coordinator shutdown begins.
    /// `None` means the future was cancelled and the run loop should return.
    pub async fn until_cancelled<F: Future>(&self, future: F) -> Option<F::Output> {
        if self.cancel.is_cancelled() {
            return None;
        }
        futures_lite::future::race(async { Some(future.await) }, async {
            self.cancel.wait().await;
            None
        })
        .await
    }

    /// Whether coordinator shutdown has begun.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// A self-driven system. The coordinator spawns [`run`](Self::run) once and
/// never ticks it; the system paces itself with a timer or by awaiting its
/// inputs through the ring `Notifier`.
#[allow(async_fn_in_trait)]
pub trait AsyncSystem: System {
    /// The system's own loop; returns when shutting down. It awaits inputs
    /// (`Input::recv`) or sleeps on a timer, doing its work on each wake, and
    /// publishes through the non-blocking output path (a full ring drops the
    /// record rather than suspending the loop).
    /// `input` is `&mut` for the same reason as
    /// [`CyclicSystem::execute`](crate::CyclicSystem::execute).
    async fn run(
        &mut self,
        context: &AsyncContext,
        input: &mut Self::Input,
        output: &mut Self::Output,
    );

    /// This system's self-description for wiring: the wired ports per
    /// direction, plus the merged capability set of both bundles.
    fn descriptor() -> SystemDescriptor {
        descriptor_for::<Self::Input, Self::Output>(Self::NAME, SystemKind::Async)
    }

    /// This instance's descriptor. Override it when the port set depends on
    /// the instance's config, such as minting one output per configured
    /// message. The builder registers this value, so a config-derived port is
    /// sized, wired, and telemetered like any static one.
    fn instance_descriptor(&self) -> SystemDescriptor {
        Self::descriptor()
    }
}

#[cfg(test)]
mod tests;
