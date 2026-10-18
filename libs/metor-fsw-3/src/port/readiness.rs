//! Waits for an async input to have a record.
//!
//! Each input allocates its wait slots when it is created and reuses them, so
//! waiting does not allocate. The slots are cleared whenever a wait ends or is
//! dropped.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use maitake_sync::wait_queue::WaitOwned;
use metor_fsw_3_ring::{Notifier, View, WakeSink};

use super::{DynInputs, Input};
use crate::record::Payload;

/// One wait slot per producer, each beside the [`Notifier`] that re-arms it.
pub(super) struct Waiters {
    slots: Vec<(Notifier, Pin<Box<Option<WaitOwned>>>)>,
}

impl Waiters {
    /// Clones each view's notifier. Views that wait on no notifier get no slot.
    pub(super) fn new<W: WakeSink>(views: &[View<W>]) -> Self {
        let slots = views
            .iter()
            .filter_map(|view| view.wake().notifier())
            .map(|notifier| (notifier.clone(), Box::pin(Some(notifier.wait_owned()))))
            .collect();
        Self { slots }
    }

    fn poll(&mut self, cx: &mut Context<'_>) {
        for (notifier, slot) in &mut self.slots {
            if slot.is_none() {
                slot.as_mut().set(Some(notifier.wait_owned()));
            }
            if let Some(wait) = slot.as_mut().as_pin_mut()
                && wait.poll(cx).is_ready()
            {
                slot.as_mut().set(None);
                // Rearm on the next poll, including after a wake without data.
                cx.waker().wake_by_ref();
            }
        }
    }

    fn clear(&mut self) {
        for (_, slot) in &mut self.slots {
            slot.as_mut().set(None);
        }
    }
}

pub(super) trait Readiness {
    type Output;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Self::Output>;
    fn clear_waiters(&mut self);
}

pub(super) async fn wait<R: Readiness>(source: &mut R) -> R::Output {
    let waiting = Waiting(source);
    core::future::poll_fn(|cx| waiting.0.poll_ready(cx)).await
}

struct Waiting<'a, R: Readiness>(&'a mut R);

impl<R: Readiness> Drop for Waiting<'_, R> {
    fn drop(&mut self) {
        self.0.clear_waiters();
    }
}

impl<T: Payload + ?Sized> Readiness for Input<T, Notifier> {
    type Output = usize;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<usize> {
        if let Some(at) = self.ready() {
            return Poll::Ready(at);
        }
        self.waiters.poll(cx);
        // Writers may publish while notification waits are being armed.
        self.ready().map_or(Poll::Pending, Poll::Ready)
    }

    fn clear_waiters(&mut self) {
        self.waiters.clear();
    }
}

impl Readiness for DynInputs<Notifier> {
    type Output = ();

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        for (_, input) in &mut self.ports {
            if input.poll_ready(cx).is_ready() {
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }

    fn clear_waiters(&mut self) {
        for (_, input) in &mut self.ports {
            input.clear_waiters();
        }
    }
}
