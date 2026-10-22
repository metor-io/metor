use metor_fsw_3_ring::{NoWake, RingBuffer};
use zerocopy::IntoBytes;

use super::*;
use crate::io::host::wire::Wire;
use crate::io::host::{Backed, Dispatch, MemberDef, dispatch::Resource, host};
use crate::io::{Cqe, DEFAULT_OPS, Op, RawHandle, ResourceEnd, SimKind, Span, Sqe, Token};
use crate::port::Input;
use crate::record::{Bytes, Record};
use crate::tests::utils::{input, pair, ring};

/// One member on a sim GPIO line, its SQ, CQ, and the step's log.
struct Rig {
    step: IoStep,
    io: IoState,
    /// Raw, so a test can write a record that does not decode.
    sq: Output<Bytes>,
    cq: Input<Cqe>,
    log: Input<LogEvent>,
    _rings: [RingBuffer; 3],
}

const OPS: usize = DEFAULT_OPS as usize;
const SQ_PER_PASS: usize = Sqe::depth(DEFAULT_OPS);

fn rig(sq_depth: usize, cq_depth: usize) -> Rig {
    rig_with(DEFAULT_OPS, sq_depth, cq_depth)
}

/// The rig, with a member that may have `ops` ops in flight.
fn rig_with(ops: u16, sq_depth: usize, cq_depth: usize) -> Rig {
    let mut io = IoState::with_members(["a"].into_iter());
    let line = Resource {
        id: "line".into(),
        backed: Backed::Wire(0),
        allow: Box::default(),
    };
    let wire = Wire::new(SimKind::GpioLine, usize::from(ops));
    let member = MemberDef {
        ops,
        grants: vec![(0.into(), ResourceEnd::Host)],
    };
    let dispatch = Dispatch::new(vec![member], vec![line], vec![wire]).unwrap();
    let sq_ring = ring::<Sqe>(sq_depth, 1);
    let sq = Output::with_max_len(sq_ring.writer(NoWake).unwrap(), Sqe::MAX_LEN);
    let (cq_ring, cq_out, cq) = pair::<Cqe>(cq_depth);
    let held = dispatch.held_for(&[0]);
    io.host = Some(Host {
        dispatch,
        os: host(usize::from(ops)).unwrap(),
        queues: [MemberQueue {
            sq: Some(input::<Sqe>(&sq_ring)),
            cq: Some(cq_out),
        }]
        .into(),
    });
    let (log_ring, log_out, log) = pair::<LogEvent>(16);
    let step = IoStep {
        lowers: vec![0],
        posts: vec![true],
        status: None,
        log: LogPort::new(log_out),
        done: Vec::with_capacity(Sqe::depth(ops)),
        held,
    };
    Rig {
        step,
        io,
        sq,
        cq,
        log,
        _rings: [sq_ring, cq_ring, log_ring],
    }
}

/// A GPIO set in `slot`, which the wire finishes at once.
fn set(slot: u16) -> Sqe {
    Sqe::new(
        Token::new(slot, 1),
        RawHandle::new(0, 0),
        &Op::GpioSet(true),
        Span::default(),
    )
}

/// An op on a handle the member does not hold, refused `Stale`.
fn stale(slot: u16) -> Sqe {
    Sqe::new(
        Token::new(slot, 1),
        RawHandle::new(9, 0),
        &Op::GpioGet,
        Span::default(),
    )
}

impl Rig {
    fn pass(&mut self, sqes: impl IntoIterator<Item = Sqe>) {
        self.pass_at(Timestamp(1), sqes);
    }

    fn pass_at(&mut self, now: Timestamp, sqes: impl IntoIterator<Item = Sqe>) {
        sqes.into_iter()
            .for_each(|sqe| self.sq.write(sqe.as_bytes()).unwrap());
        self.step.execute(now, &mut StepCx::new(&mut self.io));
    }

    fn slots(&mut self) -> Vec<u16> {
        self.cq.drain().map(|c| c.unwrap().token.index()).collect()
    }

    fn faults(&mut self) -> Vec<String> {
        let kind = |l: LogEvent| l.fields.into_iter().find(|f| f.0 == "kind");
        let lines = self.log.drain().map(|l| l.unwrap());
        lines.filter_map(kind).map(|f| f.1.into_owned()).collect()
    }
}

#[test]
fn a_pass_posts_what_it_lowered() {
    let mut r = rig(64, 64);
    r.pass((0..4).map(set));
    assert_eq!(r.slots(), [0, 1, 2, 3]);
    assert!(r.faults().is_empty());
}

#[test]
fn a_full_cq_holds_completions_for_the_next_pass() {
    let mut r = rig(64, 2);
    r.pass((0..OPS as u16).map(set));
    let mut got = r.slots();
    assert!(got.len() < OPS, "the CQ took {}", got.len());
    while got.len() < OPS {
        r.pass([]);
        got.extend(r.slots());
    }
    assert_eq!(got, (0..OPS as u16).collect::<Vec<_>>());
    assert!(r.faults().is_empty());
}

#[test]
fn a_cq_that_stays_full_loses_completions_and_faults() {
    let mut r = rig(64, 2);
    r.pass((0..SQ_PER_PASS).map(|_| stale(0)));
    assert!(
        r.faults().is_empty(),
        "the first pass holds what the CQ refuses"
    );
    r.pass((0..SQ_PER_PASS).map(|_| stale(0)));
    assert_eq!(r.faults(), ["io_cq_full"]);
}

#[test]
fn a_pass_lowers_at_most_two_records_per_slot() {
    let mut r = rig(64, 64);
    r.pass((0..SQ_PER_PASS + 8).map(|_| stale(0)));
    assert_eq!(r.slots().len(), SQ_PER_PASS);
    r.pass([]);
    assert_eq!(r.slots().len(), 8, "the rest wait in the SQ");
}

#[test]
fn a_pass_lowers_by_the_members_budget() {
    let mut r = rig_with(40, 128, 128);
    r.pass((0..90).map(|_| stale(0)));
    assert_eq!(r.slots().len(), 80);
    r.pass([]);
    assert_eq!(r.slots().len(), 10);
}

/// A budget-40 burst into a 16-deep CQ waits in the dispatch, then reaches
/// the CQ over later passes with none lost.
#[test]
fn a_burst_past_the_cq_reaches_it_over_passes() {
    let mut r = rig_with(40, 128, Cqe::DEPTH);
    r.pass((0..40).map(set));
    let mut got = r.slots();
    assert!(got.len() < 40, "the CQ took {}", got.len());
    for _ in 0..4 {
        r.pass([]);
        got.extend(r.slots());
    }
    assert_eq!(got, (0..40).collect::<Vec<u16>>());
    assert!(r.faults().is_empty(), "nothing lost");
}

#[test]
fn a_pass_lands_the_timeouts_its_clock_passes() {
    let mut r = rig(64, 64);
    let after = core::time::Duration::from_micros(20);
    let timeout = Sqe::new(
        Token::new(3, 1),
        RawHandle::new(0, 0),
        &Op::Timeout(after),
        Span::default(),
    );
    r.pass_at(Timestamp(10), [timeout]);
    r.pass_at(Timestamp(29), []);
    assert!(r.slots().is_empty());
    r.pass_at(Timestamp(30), []);
    assert_eq!(r.slots(), [3]);
}

/// Timeouts landing past a full CQ wait in the dispatch, none lost.
#[test]
fn timeouts_past_a_full_cq_land_on_later_passes() {
    let mut r = rig_with(40, 128, Cqe::DEPTH);
    let timeout = |slot| {
        let after = core::time::Duration::from_micros(5);
        Sqe::new(
            Token::new(slot, 1),
            RawHandle::new(0, 0),
            &Op::Timeout(after),
            Span::default(),
        )
    };
    r.pass_at(Timestamp(0), (0..40).map(timeout));
    assert!(r.slots().is_empty());
    r.pass_at(Timestamp(5), []);
    let mut got = r.slots();
    assert!(got.len() < 40, "the CQ took {}", got.len());
    for turn in 6..10 {
        r.pass_at(Timestamp(turn), []);
        got.extend(r.slots());
    }
    got.sort_unstable();
    assert_eq!(got, (0..40).collect::<Vec<u16>>());
}

#[test]
fn a_budget_of_one_lowers_two_records() {
    let mut r = rig_with(1, 64, 64);
    r.pass((0..3).map(|_| stale(0)));
    assert_eq!(r.slots().len(), 2);
}

#[test]
fn bad_records_fault() {
    let mut r = rig(64, 64);
    r.pass([Sqe::new(
        Token::new(DEFAULT_OPS, 1),
        RawHandle::new(0, 0),
        &Op::GpioGet,
        Span::default(),
    )]);
    r.sq.write(&[1, 2, 3]).unwrap();
    r.pass([]);
    assert_eq!(r.faults(), ["io_rejected", "io_corrupt"]);
    assert!(r.slots().is_empty());
}
