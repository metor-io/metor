//! Unit tests for the sequence runtime, exercised against a hand-stepped
//! [`CycleClock`] rather than a real coordinator. Covers the [`Wait`] future
//! (deadline resolution and cancel short-circuit), the free
//! [`wait`]/[`progress`]/[`aborted`] author API, and the [`cycle`]
//! suspension point of async-fn systems.

use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;
use std::time::Duration;

use metor_proto::types::Timestamp;

use crate::sequence::{
    CycleClock, Step, Wait, aborted, current, cycle, progress, wait, with_clock,
};

/// Poll a [`Wait`] once with a no-op waker.
///
/// `Wait` is `Unpin`, so a stack pin suffices. Callers must be inside
/// [`with_clock`], since the future reads the ambient clock.
fn poll(w: &mut Wait) -> Poll<Step> {
    let mut cx = Context::from_waker(Waker::noop());
    <Wait as core::future::Future>::poll(Pin::new(w), &mut cx)
}

#[test]
fn wait_elapses_at_deadline() {
    let clock = Rc::new(CycleClock::default());
    clock.now.set(Timestamp(0));
    with_clock(&clock, || {
        // Timestamp is in microseconds, so the deadline is now(0) + 5.
        let mut w = wait(Duration::from_micros(5));
        assert_eq!(poll(&mut w), Poll::Pending, "pending before the deadline");
        clock.now.set(Timestamp(4));
        assert_eq!(poll(&mut w), Poll::Pending, "still before the deadline");
        clock.now.set(Timestamp(5));
        assert_eq!(
            poll(&mut w),
            Poll::Ready(Step::Elapsed),
            "elapsed at the deadline"
        );
    });
}

#[test]
fn cancel_short_circuits_wait() {
    let clock = Rc::new(CycleClock::default());
    clock.now.set(Timestamp(0));
    with_clock(&clock, || {
        let mut w = wait(Duration::from_micros(100));
        assert_eq!(poll(&mut w), Poll::Pending, "far from the deadline");
        clock.cancel.set(true);
        match poll(&mut w) {
            Poll::Ready(step) => assert!(step.aborted(), "cancel resolves to an aborted step"),
            Poll::Pending => panic!("cancel must short-circuit the wait"),
        }
    });
}

#[test]
fn progress_and_aborted_free_fns() {
    let clock = Rc::new(CycleClock::default());
    with_clock(&clock, || {
        progress("warming up");
        progress(String::from("ready"));
        assert!(!aborted(), "not cancelled yet");
        clock.cancel.set(true);
        assert!(aborted(), "cancel observed by the free fn");
    });
    // The lines accumulate in the clock and drain out after the poll.
    assert_eq!(
        clock.drain_progress(),
        vec!["warming up".to_string(), "ready".to_string()]
    );
    assert!(
        clock.drain_progress().is_empty(),
        "draining empties the buffer"
    );
}

#[test]
fn now_reads_the_stepped_ambient_clock() {
    // `now()` is the cycle time the clock was last refreshed with, never wall
    // time, so a sequence stamps its emitted frames deterministically.
    let clock = Rc::new(CycleClock::default());
    clock.now.set(Timestamp(42));
    with_clock(&clock, || {
        assert_eq!(super::now(), Timestamp(42));
    });
    // A refresh between polls is observed by the next poll.
    clock.now.set(Timestamp(43));
    with_clock(&clock, || assert_eq!(super::now(), Timestamp(43)));
}

#[test]
fn clock_is_cleared_after_with_clock() {
    let clock = Rc::new(CycleClock::default());
    assert!(current().is_none(), "no ambient clock outside a poll");
    with_clock(&clock, || {
        assert!(current().is_some(), "clock is ambient inside")
    });
    assert!(current().is_none(), "ambient clock cleared on the way out");
}


/// `cycle()` arms at the current clock and resolves only once a LATER cycle
/// polls it — never the same cycle, so a `loop { cycle().await; … }` body
/// runs exactly once per coordinator cycle, deterministically.
#[test]
fn cycle_resolves_on_the_next_cycle() {
    let clock = Rc::new(CycleClock::default());
    clock.now.set(Timestamp(10));
    let mut fut = with_clock(&clock, cycle);
    let mut cx = Context::from_waker(Waker::noop());
    let mut poll = |clock: &Rc<CycleClock>, fut: &mut super::NextCycle| {
        with_clock(clock, || {
            core::future::Future::poll(Pin::new(fut), &mut cx)
        })
    };
    assert_eq!(poll(&clock, &mut fut), Poll::Pending, "same cycle: pending");
    clock.now.set(Timestamp(11));
    assert_eq!(
        poll(&clock, &mut fut),
        Poll::Ready(Timestamp(11)),
        "next cycle: ready with that cycle's now"
    );
}
