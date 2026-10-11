//! Reusable notification waits, cleared when a readiness future ends.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use metor_fsw_3_ring::{Notifier, View, WakeSink};
use stellarator::sync::wait_queue::WaitOwned;

use super::{DynInputs, Input};
use crate::record::Record;

pub(super) struct Waiters {
    slots: Vec<Pin<Box<Option<WaitOwned>>>>,
}

impl Waiters {
    pub(super) fn new<W: WakeSink>(views: &[View<W>]) -> Self {
        let slots = views
            .iter()
            .filter_map(|view| view.wake().wait_owned())
            .map(|wait| Box::pin(Some(wait)))
            .collect();
        Self { slots }
    }

    fn poll<'a>(&mut self, wakes: impl Iterator<Item = &'a Notifier>, cx: &mut Context<'_>) {
        for (slot, wake) in self.slots.iter_mut().zip(wakes) {
            if slot.is_none() {
                slot.as_mut().set(Some(wake.wait_owned()));
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
        for slot in &mut self.slots {
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

impl<T: Record + ?Sized> Readiness for Input<T, Notifier> {
    type Output = usize;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<usize> {
        if let Some(at) = self.ready() {
            return Poll::Ready(at);
        }
        self.waiters.poll(self.views.iter().map(View::wake), cx);
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
