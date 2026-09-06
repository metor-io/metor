//! Free-running systems managed by the host executor.
//!
//! The coordinator spawns each [`AsyncSystem`] once. Its task owns pacing,
//! input waits, and status publication; [`AsyncContext`] provides cancellation.

use core::future::Future;

use metor_fsw_2_core::{StatusPort, System, SystemDescriptor, SystemKind, descriptor_for};

/// An [`AsyncSystem`]'s run-loop handles: cancellation-aware waits, and its
/// own status record.
pub struct AsyncContext {
    pub(crate) cancel: stellarator::util::CancelToken,
    pub(crate) status: StatusPort,
}

impl AsyncContext {
    /// The system's `system_status` record. The host never steps a
    /// free-running system, so it publishes this itself:
    /// `context.status().tick(elapsed_us)` once per loop iteration.
    pub fn status(&mut self) -> &mut StatusPort {
        &mut self.status
    }

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
        context: &mut AsyncContext,
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
