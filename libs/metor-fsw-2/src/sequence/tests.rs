//! Sequence-runtime unit tests (no dlopen): the [`Wait`] future under a hand-made
//! [`SeqClock`] (deadline-vs-`now`, cancel short-circuit), the free
//! [`wait`]/[`progress`]/[`aborted`] author API, and a real `#[sequence]`
//! macro-expansion asserting the generated `descriptor()` shape.

use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;
use std::time::Duration;

use metor_fsw_ring::Backing;
use metor_proto::types::Timestamp;

use crate::sequence::{
    Outcome, SeqClock, SeqSystem, Step, Wait, aborted, current, progress, wait, with_clock,
};
use crate::{Frame, Input, Output, SlotControlIn, SequenceStatus, SystemHealth, SystemKind, SystemLog};

/// Poll a [`Wait`] once with a no-op waker — it is `Unpin` (a bare `Timestamp`), so a
/// stack pin is fine. Must be called inside [`with_clock`] (the future reads the
/// ambient clock).
fn poll(w: &mut Wait) -> Poll<Step> {
    let mut cx = Context::from_waker(Waker::noop());
    <Wait as core::future::Future>::poll(Pin::new(w), &mut cx)
}

#[test]
fn wait_elapses_at_deadline() {
    let clock = Rc::new(SeqClock::default());
    clock.now.set(Timestamp(0));
    with_clock(&clock, || {
        // Deadline = now(0) + 5us = 5 (micros — the Timestamp unit).
        let mut w = wait(Duration::from_micros(5));
        assert_eq!(poll(&mut w), Poll::Pending, "before the deadline → pending");
        clock.now.set(Timestamp(4));
        assert_eq!(poll(&mut w), Poll::Pending, "still before the deadline");
        clock.now.set(Timestamp(5));
        assert_eq!(poll(&mut w), Poll::Ready(Step::Elapsed), "at the deadline → elapsed");
    });
}

#[test]
fn cancel_short_circuits_wait() {
    let clock = Rc::new(SeqClock::default());
    clock.now.set(Timestamp(0));
    with_clock(&clock, || {
        let mut w = wait(Duration::from_micros(100));
        assert_eq!(poll(&mut w), Poll::Pending, "far from the deadline");
        clock.cancel.set(true);
        match poll(&mut w) {
            Poll::Ready(step) => assert!(step.aborted(), "cancel → Step::Aborted"),
            Poll::Pending => panic!("cancel must short-circuit the wait"),
        }
    });
}

#[test]
fn progress_and_aborted_free_fns() {
    let clock = Rc::new(SeqClock::default());
    with_clock(&clock, || {
        progress("warming up");
        progress(String::from("ready"));
        assert!(!aborted(), "not cancelled yet");
        clock.cancel.set(true);
        assert!(aborted(), "cancel observed by the free fn");
    });
    // Progress drains out of the clock after the poll.
    assert_eq!(
        clock.drain_progress(),
        vec!["warming up".to_string(), "ready".to_string()]
    );
    assert!(clock.drain_progress().is_empty(), "draining empties the buffer");
}

#[test]
fn now_reads_the_stepped_ambient_clock() {
    // E7: `now()` is the coordinator cycle time, refreshed before each poll — a
    // sequence stamps its emitted frames with it (never wall time).
    let clock = Rc::new(SeqClock::default());
    clock.now.set(Timestamp(42));
    with_clock(&clock, || {
        assert_eq!(super::now(), Timestamp(42));
        let seq = super::Seq::new(clock.clone());
        assert_eq!(seq.now(), Timestamp(42), "the handle reads the same clock");
    });
    // The next cycle's refresh is observed by the next poll.
    clock.now.set(Timestamp(43));
    with_clock(&clock, || assert_eq!(super::now(), Timestamp(43)));
}

#[test]
fn clock_is_cleared_after_with_clock() {
    let clock = Rc::new(SeqClock::default());
    assert!(current().is_none(), "no ambient clock outside a poll");
    with_clock(&clock, || assert!(current().is_some(), "clock is ambient inside"));
    assert!(current().is_none(), "ambient clock cleared on the way out");
}

// ---------------------------------------------------------------------------
// A real `#[sequence]` expansion: the generated `descriptor()` must list the user
// ports plus the implicit `SlotControlIn` input and the `SequenceStatus`+health/log
// output tail, in that order, kind = Cyclic. (Existing Frame types are reused for the
// ports so the test needs no new frames.) The `fsw_*` exports are `#[cfg(not(test))]`,
// so they do not collide with the abi test's `export_system!(Counter)`.
// ---------------------------------------------------------------------------

#[crate::sequence]
async fn demo<B: Backing>(att: Input<SystemHealth, B>, mut out: Output<SystemHealth, B>) -> Outcome {
    // The owned ports are really moved into the future (plain method calls on owned
    // values — no per-cycle re-borrow).
    let _ = att;
    let _ = out.write(&SystemHealth {
        timestamp: Timestamp(0),
        cycles: 0,
        errors: 0,
        lapped_inputs: 0,
        last_execute_micros: 0,
        error_counts: crate::FrameMap::EMPTY,
    });
    Outcome::Completed
}

#[test]
fn macro_descriptor_shape() {
    let d = <__Seq_demo as SeqSystem>::descriptor();

    assert_eq!(d.name, "demo", "name defaults to the fn ident");
    assert_eq!(d.kind, SystemKind::Cyclic);

    // inputs = [user `att` (SystemHealth), then the implicit SlotControlIn].
    assert_eq!(d.inputs.len(), 2);
    assert_eq!(d.inputs[0].id.component().expect("table port"), SystemHealth::FRAME_ID);
    assert_eq!(d.inputs[1].id.component().expect("table port"), SlotControlIn::FRAME_ID);

    // outputs = [user `out` (SystemHealth), then the Out<SeqStatusOut> tail:
    // SequenceStatus, health, log].
    assert_eq!(d.outputs.len(), 4);
    assert_eq!(d.outputs[0].id.component().expect("table port"), SystemHealth::FRAME_ID);
    assert_eq!(d.outputs[1].id.component().expect("table port"), SequenceStatus::FRAME_ID);
    assert_eq!(d.outputs[2].id.component().expect("table port"), SystemHealth::FRAME_ID);
    assert_eq!(d.outputs[3].id.component().expect("table port"), SystemLog::FRAME_ID);
}
