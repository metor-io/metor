use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};

use metor_proto::types::Timestamp;

use super::allocations;
use crate::io::arena::Arena;
use crate::io::host::dispatch::Rejected;
use crate::io::host::dispatch::Resource;
use crate::io::host::{Backed, Class, Dispatch, OpId, OsDone, OsOp, Out, Route};
use crate::io::status::ResourceStatus;
use crate::io::*;
use crate::record::Record;
use crate::tests::utils::pair;

fn null() -> OwnedFd {
    File::open("/dev/null").unwrap().into()
}

/// Member 0 holds the real resource 0 and the sim resource 1; member 1 holds
/// resource 0.
struct Rig {
    d: Dispatch,
    arenas: [Arena; 2],
    fd: i32,
}

fn rig() -> Rig {
    let fd = null();
    let raw = fd.as_raw_fd();
    let resources = vec![
        Resource {
            id: "tty".into(),
            sim: false,
            backed: Backed::Fd(fd, Class::Tty),
        },
        Resource {
            id: "sim".into(),
            sim: true,
            backed: Backed::Sim(4),
        },
    ];
    Rig {
        d: Dispatch::new(vec![vec![0, 1], vec![0]], resources).unwrap(),
        arenas: [super::arena(64), super::arena(64)],
        fd: raw,
    }
}

const TTY: RawHandle = RawHandle::new(0, 0);
const SIM: RawHandle = RawHandle::new(1, 0);

fn sqe(slot: u16, handle: RawHandle, op: Op, buf: Span) -> Sqe {
    Sqe::new(Token::new(slot, 7), handle, &op, buf)
}

fn read(slot: u16, handle: RawHandle) -> Sqe {
    sqe(slot, handle, Op::Read { offset: None }, Span::new(8, 16))
}

impl Rig {
    fn lower(&mut self, m: usize, sqe: Sqe) {
        assert_eq!(self.d.lower(m, sqe, &self.arenas[m]), Ok(()));
    }

    fn routes(&mut self) -> Vec<Route> {
        self.d.drain_routes().collect()
    }

    fn replies(&mut self) -> Vec<(usize, Cqe)> {
        self.d.replies().collect()
    }

    fn only_os(&mut self) -> OsOp {
        let mut routes = self.routes();
        match (routes.pop(), routes.is_empty()) {
            (Some(Route::Os(op)), true) => op,
            other => panic!("{other:?}"),
        }
    }

    fn done(&mut self, id: OpId, result: Result<Out, IoError>) -> Option<(usize, Cqe)> {
        self.d.raise(OsDone { id, result })
    }
}

#[test]
fn real_op_resolves_its_descriptor_buffer_and_index() {
    let mut r = rig();
    r.lower(1, read(3, TTY));
    let op = r.only_os();
    assert_eq!((op.fd, op.class), (r.fd, Class::Tty));
    assert_eq!(op.id, OpId::new((MAX_OPS + 3) as u16, 7));
    assert_eq!(op.buf.unwrap().len(), 16);
    let done = r.done(op.id, Ok(Out::Len(5)));
    assert_eq!(done, Some((1, Cqe::ok(Token::new(3, 7), 5))));
    assert_eq!(r.done(op.id, Ok(Out::Len(5))), None, "raised once");
}

#[test]
fn sim_op_goes_to_its_port_under_its_op_id() {
    let mut r = rig();
    r.lower(0, read(2, SIM));
    let [Route::Sim(4, req)] = r.routes()[..] else {
        panic!()
    };
    assert_eq!(req.token, Token::new(2, 7), "member 0's id is its token");
    assert_eq!(req.buf, Span::new(8, 16));
    let reply = r.d.raise_sim(Cqe::err(req.token, IoError::Device));
    assert_eq!(
        reply,
        Some((0, Cqe::err(Token::new(2, 7), IoError::Device)))
    );
}

#[test]
fn refusals_reply_without_running() {
    let mut r = rig();
    let cases = [
        (
            sqe(0, RawHandle::new(9, 0), Op::Fsync, Span::default()),
            IoError::Stale,
        ),
        (
            sqe(0, RawHandle::new(0, 1), Op::Fsync, Span::default()),
            IoError::Stale,
        ),
        (sqe(0, TTY, Op::Recv, Span::new(60, 8)), IoError::BadBuf),
    ];
    for (sqe, e) in cases {
        r.lower(0, sqe);
        assert_eq!(r.replies(), [(0, Cqe::err(sqe.token, e))]);
    }
    let mut bad = sqe(0, TTY, Op::Fsync, Span::default());
    bad.opcode = 0xEE;
    r.lower(0, bad);
    assert_eq!(
        r.replies(),
        [(0, Cqe::err(bad.token, IoError::Unsupported))]
    );
    assert!(r.routes().is_empty());
}

#[test]
fn a_token_that_is_not_a_free_slot_is_dropped_unanswered() {
    let mut r = rig();
    r.lower(0, read(1, TTY));
    let _ = r.routes();
    let reused = sqe(1, RawHandle::new(9, 0), Op::Fsync, Span::default());
    let past = sqe(MAX_OPS as u16, TTY, Op::Fsync, Span::default());
    for bad in [read(1, TTY), reused, past] {
        assert_eq!(r.d.lower(0, bad, &r.arenas[0]), Err(Rejected));
    }
    assert!(r.routes().is_empty() && r.replies().is_empty());
}

#[test]
fn teardown_waits_only_for_ops_on_descriptors() {
    let mut r = rig();
    r.lower(0, read(0, SIM));
    let _ = r.routes();
    assert!(!r.d.live(), "a model holds the op");
    r.lower(0, read(1, TTY));
    assert!(r.d.live());
}

#[test]
fn closed_grant_is_stale_until_a_remake() {
    let mut r = rig();
    r.lower(0, sqe(0, TTY, Op::Close, Span::default()));
    assert_eq!(r.replies(), [(0, Cqe::ok(Token::new(0, 7), 0))]);
    assert!(r.routes().is_empty(), "the resource stays open");
    r.lower(0, sqe(1, TTY, Op::Fsync, Span::default()));
    assert_eq!(r.replies()[0].1.result(), Err(IoError::Stale));
    r.d.quiesce_member(0);
    r.d.member_drained(0);
    r.d.end_queiscing_member(0);
    r.lower(0, sqe(1, TTY, Op::Fsync, Span::default()));
    assert_eq!(r.routes().len(), 1);
}

#[test]
fn accepted_stream_is_a_derived_handle() {
    let mut r = rig();
    r.lower(0, sqe(0, TTY, Op::Accept, Span::default()));
    let op = r.only_os();
    let (_, cqe) = r
        .done(op.id, Ok(Out::Opened(null(), Class::Stream)))
        .unwrap();
    let stream = RawHandle::from_raw(cqe.result().unwrap());
    assert_eq!(
        stream,
        RawHandle::new(2, 1),
        "past the grants, first generation"
    );
    r.lower(0, sqe(1, stream, Op::Recv, Span::new(0, 4)));
    assert_eq!(r.only_os().class, Class::Stream);
    r.lower(0, sqe(2, stream, Op::Close, Span::default()));
    assert!(matches!(r.routes()[..], [Route::Close(_)]));
    r.lower(0, sqe(3, stream, Op::Fsync, Span::default()));
    assert_eq!(r.replies().last().unwrap().1.result(), Err(IoError::Stale));
}

#[test]
fn full_derived_table_closes_and_refuses() {
    let mut r = rig();
    let accept = |r: &mut Rig, slot: usize| {
        r.lower(0, sqe(slot as u16, TTY, Op::Accept, Span::default()));
        let op = r.only_os();
        r.done(op.id, Ok(Out::Opened(null(), Class::Stream)))
            .unwrap()
            .1
    };
    for slot in 0..MAX_OPS {
        assert!(accept(&mut r, slot).result().is_ok());
    }
    assert_eq!(accept(&mut r, 0).result(), Err(IoError::Refused));
    assert!(matches!(r.routes()[..], [Route::Close(_)]));
}

#[test]
fn cancel_goes_out_once() {
    let mut r = rig();
    r.lower(0, read(0, TTY));
    r.lower(0, read(1, SIM));
    let _ = r.routes();
    let cancel = |slot| sqe(slot, TTY, Op::Cancel, Span::default());
    for slot in [0, 0, 1, 5] {
        r.lower(0, cancel(slot));
    }
    let routes = r.routes();
    assert!(matches!(routes[0], Route::Cancel(id) if id == OpId::new(0, 7)));
    let Route::Sim(4, req) = routes[1] else {
        panic!()
    };
    assert_eq!((req.op(), req.token), (Ok(Op::Cancel), Token::new(1, 7)));
    assert_eq!(routes.len(), 2);
    assert!(r.replies().is_empty(), "a cancel gets no completion");
}

#[test]
fn quiesce_cancels_drops_and_waits() {
    let mut r = rig();
    r.lower(0, read(0, TTY));
    r.lower(0, sqe(1, TTY, Op::Accept, Span::default()));
    let ids: Vec<OpId> = r
        .routes()
        .into_iter()
        .map(|route| match route {
            Route::Os(op) => op.id,
            other => panic!("{other:?}"),
        })
        .collect();
    r.d.quiesce_member(0);
    assert_eq!(r.routes().len(), 2, "one cancel per op");
    r.d.quiesce_member(0);
    assert!(r.routes().is_empty(), "never twice");
    r.lower(0, read(2, TTY));
    assert!(
        r.routes().is_empty() && r.replies().is_empty(),
        "new ops are dropped"
    );
    assert!(!r.d.member_idle(0));
    r.d.member_drained(0);
    assert_eq!(r.done(ids[0], Err(IoError::Canceled)), None);
    assert!(!r.d.member_idle(0));
    assert_eq!(r.done(ids[1], Ok(Out::Opened(null(), Class::Stream))), None);
    assert!(
        matches!(r.routes()[..], [Route::Close(_)]),
        "nothing will close it"
    );
    assert!(r.d.member_idle(0));
    assert!(r.d.member_idle(1), "a member not quiescing is idle");
}

#[test]
fn drain_closes_derived_handles() {
    let mut r = rig();
    r.lower(0, sqe(0, TTY, Op::Accept, Span::default()));
    let op = r.only_os();
    r.done(op.id, Ok(Out::Opened(null(), Class::Stream)));
    r.d.quiesce_member(0);
    r.d.member_drained(0);
    r.d.member_drained(0);
    assert!(matches!(r.routes()[..], [Route::Close(_)]));
}

#[test]
fn report_writes_changed_resources() {
    let mut r = rig();
    r.lower(0, read(0, TTY));
    let op = r.only_os();
    r.done(op.id, Ok(Out::Len(4)));
    let (_ring, out, mut inp) = pair::<ResourceStatus>(ResourceStatus::DEPTH);
    r.d.report(Timestamp(1), &out);
    let got: Vec<ResourceStatus> = inp.drain().map(|s| *s.unwrap()).collect();
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].submitted, got[0].completed, got[0].bytes_in),
        (1, 1, 4)
    );
    r.d.report(Timestamp(2), &out);
    assert_eq!(inp.drain().count(), 0, "unchanged is not written");
}

#[test]
fn lowering_and_raising_allocate_nothing() {
    let mut r = rig();
    let mut ids = Vec::with_capacity(4);
    let n = allocations(|| {
        for _ in 0..8 {
            r.lower(0, read(0, TTY));
            ids.extend(r.d.drain_routes().map(|route| match route {
                Route::Os(op) => op.id,
                _ => OpId::new(0, 0),
            }));
            let id = ids.pop().unwrap();
            r.d.raise(OsDone {
                id,
                result: Ok(Out::Len(1)),
            });
        }
    });
    assert_eq!(n, 0);
}
