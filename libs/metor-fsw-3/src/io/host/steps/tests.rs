use metor_fsw_3_ring::{NoWake, RingBuffer};
use zerocopy::IntoBytes;

use super::*;
use crate::io::host::wire::Wire;
use crate::io::host::{Backed, Dispatch, dispatch::Resource, host};
use crate::io::{Op, RawHandle, ResourceEnd, SimKind, Span, Token};
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

fn rig(sq_depth: usize, cq_depth: usize) -> Rig {
    let mut io = IoState::with_members(["a"].into_iter());
    let line = Resource {
        id: "line".into(),
        backed: Backed::Wire(0),
    };
    let wire = Wire::new(SimKind::GpioLine, MAX_OPS);
    let dispatch = Dispatch::new(
        vec![vec![(0.into(), ResourceEnd::Host)]],
        vec![line],
        vec![wire],
    )
    .unwrap();
    io.host = Some(Host {
        dispatch,
        os: host(MAX_OPS).unwrap(),
    });
    let sq_ring = ring::<Sqe>(sq_depth, 1);
    let sq = Output::with_max_len(sq_ring.writer(NoWake).unwrap(), Sqe::MAX_LEN);
    let sq_in = input::<Sqe>(&sq_ring);
    let (cq_ring, cq_out, cq) = pair::<Cqe>(cq_depth);
    let (log_ring, log_out, log) = pair::<LogEvent>(16);
    let step = IoStep {
        sqs: vec![(0, sq_in)],
        cqs: vec![Some(cq_out)],
        status: None,
        log: LogPort::new(log_out),
        done: Vec::with_capacity(2 * MAX_OPS),
        held: Dispatch::held_for(1),
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
        sqes.into_iter()
            .for_each(|sqe| self.sq.write(sqe.as_bytes()).unwrap());
        self.step
            .execute(Timestamp(1), &mut StepCx::new(&mut self.io));
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
    r.pass((0..MAX_OPS as u16).map(set));
    let mut got = r.slots();
    assert!(got.len() < MAX_OPS, "the CQ took {}", got.len());
    while got.len() < MAX_OPS {
        r.pass([]);
        got.extend(r.slots());
    }
    assert_eq!(got, (0..MAX_OPS as u16).collect::<Vec<_>>());
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
fn bad_records_fault() {
    let mut r = rig(64, 64);
    r.pass([Sqe::new(
        Token::new(MAX_OPS as u16, 1),
        RawHandle::new(0, 0),
        &Op::GpioGet,
        Span::default(),
    )]);
    r.sq.write(&[1, 2, 3]).unwrap();
    r.pass([]);
    assert_eq!(r.faults(), ["io_rejected", "io_corrupt"]);
    assert!(r.slots().is_empty());
}
