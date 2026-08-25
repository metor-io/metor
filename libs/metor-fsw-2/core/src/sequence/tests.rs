//! Unit tests for the sequence runtime, exercised against a hand-stepped
//! [`CycleClock`] rather than a real coordinator. Covers the [`Wait`] future
//! (deadline resolution and cancel short-circuit), the free
//! [`wait`]/[`progress`]/[`aborted`] author API, the [`cycle`] suspension
//! point of async-fn systems, and the [`check`] combinator built on them —
//! dwell, budget, cancel precedence, and the [`Check::or_fail`] mapping a body
//! hangs its one safing site off.

use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;
use std::time::Duration;

use metor_proto::types::Timestamp;

use crate::sequence::{
    Check, CycleClock, MAX_PROGRESS, Outcome, Step, Wait, aborted, check, current, cycle, progress,
    wait, with_clock,
};

/// Drive a `check` future across hand-stepped cycles, advancing `now` by one
/// microsecond per cycle, and return how it resolved plus the cycle count it
/// took. The predicate sees the same cycle sequence a coordinator would give
/// it.
fn run_check(
    clock: &Rc<CycleClock>,
    pred: impl FnMut() -> bool,
    hold: Duration,
    timeout: Duration,
) -> (Check, u32) {
    let mut fut = Box::pin(check(pred, hold, timeout));
    let mut cx = Context::from_waker(Waker::noop());
    for polls in 1.. {
        let step = with_clock(clock, || fut.as_mut().poll(&mut cx));
        if let Poll::Ready(outcome) = step {
            return (outcome, polls);
        }
        clock.now.set(Timestamp(clock.now.get().0 + 1));
    }
    unreachable!()
}

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
fn progress_is_bounded_to_the_status_frame_capacity() {
    let clock = Rc::new(CycleClock::default());
    with_clock(&clock, || {
        for i in 0..=MAX_PROGRESS {
            progress(format!("line {i}"));
        }
    });
    assert_eq!(clock.drain_progress().len(), MAX_PROGRESS);
}

#[test]
fn now_reads_the_stepped_ambient_clock() {
    // `now()` is the cycle time the clock was last refreshed with, never wall
    // time, so a sequence stamps its emitted frames deterministically.
    let clock = Rc::new(CycleClock::default());
    clock.now.set(Timestamp(42));
    with_clock(&clock, || assert_eq!(super::now(), Timestamp(42)));
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
        with_clock(clock, || core::future::Future::poll(Pin::new(fut), &mut cx))
    };
    assert_eq!(poll(&clock, &mut fut), Poll::Pending, "same cycle: pending");
    clock.now.set(Timestamp(11));
    assert_eq!(
        poll(&clock, &mut fut),
        Poll::Ready(Timestamp(11)),
        "next cycle: ready with that cycle's now"
    );
}

/// A predicate already true resolves on the calling cycle when no dwell is
/// asked for — the "true once" case, and the reason a body can chain checks
/// without burning a cycle apiece.
#[test]
fn check_holds_immediately_without_a_dwell() {
    let clock = Rc::new(CycleClock::default());
    let (outcome, polls) = run_check(&clock, || true, Duration::ZERO, Duration::from_micros(10));
    assert_eq!(outcome, Check::Held);
    assert_eq!(polls, 1, "resolves on the calling cycle");
}

/// A dwell is satisfied by the predicate holding across cycles, not by it
/// merely being true once.
#[test]
fn check_requires_the_predicate_to_hold_for_the_dwell() {
    let clock = Rc::new(CycleClock::default());
    let (outcome, polls) = run_check(
        &clock,
        || true,
        Duration::from_micros(3),
        Duration::from_micros(100),
    );
    assert_eq!(outcome, Check::Held);
    assert_eq!(polls, 4, "one poll to arm the dwell, three to serve it");
}

/// A predicate that goes false restarts the dwell, so a flapping condition
/// never satisfies a hold it did not actually sustain.
#[test]
fn check_restarts_the_dwell_when_the_predicate_breaks() {
    let clock = Rc::new(CycleClock::default());
    // False on the third cycle: the dwell armed on cycle one is discarded and
    // only the run starting at cycle four can satisfy it.
    let mut cycle_no = 0;
    let (outcome, polls) = run_check(
        &clock,
        || {
            cycle_no += 1;
            cycle_no != 3
        },
        Duration::from_micros(2),
        Duration::from_micros(100),
    );
    assert_eq!(outcome, Check::Held);
    assert_eq!(
        polls, 6,
        "dwell re-armed on cycle four, served on cycle six"
    );
}

/// The budget is measured from the call, so a predicate that never holds gives
/// up rather than running forever.
#[test]
fn check_times_out_on_a_predicate_that_never_holds() {
    let clock = Rc::new(CycleClock::default());
    let (outcome, polls) = run_check(&clock, || false, Duration::ZERO, Duration::from_micros(3));
    assert_eq!(outcome, Check::TimedOut);
    assert_eq!(polls, 4, "evaluated on each cycle through the deadline");
}

/// A dwell that cannot complete inside the budget times out even though the
/// predicate is true — the budget covers the dwell, it does not restart for it.
#[test]
fn check_times_out_when_the_dwell_outlasts_the_budget() {
    let clock = Rc::new(CycleClock::default());
    let (outcome, _) = run_check(
        &clock,
        || true,
        Duration::from_micros(10),
        Duration::from_micros(3),
    );
    assert_eq!(outcome, Check::TimedOut);
}

/// Cancellation wins over both a satisfied predicate and a live budget, and is
/// observed before the predicate runs for that cycle.
#[test]
fn check_aborts_on_cancel() {
    let clock = Rc::new(CycleClock::default());
    clock.cancel.set(true);
    let mut ran = false;
    let (outcome, polls) = run_check(
        &clock,
        || {
            ran = true;
            true
        },
        Duration::ZERO,
        Duration::from_micros(100),
    );
    assert_eq!(outcome, Check::Aborted);
    assert_eq!(polls, 1);
    assert!(
        !ran,
        "cancel short-circuits before evaluating the predicate"
    );
}

/// A very large budget saturates rather than overflowing the microsecond
/// timestamp, which is how a body spells "no timeout".
#[test]
fn check_saturates_an_unbounded_budget() {
    let clock = Rc::new(CycleClock::default());
    clock.now.set(Timestamp(i64::MAX - 1));
    let (outcome, _) = run_check(&clock, || true, Duration::ZERO, Duration::MAX);
    assert_eq!(outcome, Check::Held, "no overflow panic on the deadline");
}

/// `or_fail` is what gives a body one safing site: it maps the three outcomes
/// onto `?`, naming the phase in a progress line only when it timed out.
#[test]
fn or_fail_maps_outcomes_and_records_the_timed_out_phase() {
    let clock = Rc::new(CycleClock::default());
    with_clock(&clock, || {
        assert_eq!(Check::Held.or_fail("warm-up"), Ok(()));
        assert_eq!(Check::TimedOut.or_fail("warm-up"), Err(Outcome::Failed));
        assert_eq!(Check::Aborted.or_fail("warm-up"), Err(Outcome::Aborted));
    });
    assert_eq!(
        clock.drain_progress(),
        vec!["timeout in warm-up".to_string()],
        "only the timeout names the phase"
    );
}
