use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

use metor_proto::types::Timestamp;

use super::allocations;
use crate::io2::arena::Arena;
use crate::io2::host::dispatch::Resource;
use crate::io2::host::{Backed, Class, Dispatch, OpId, OsDone, OsOp, Out, Route};
use crate::io2::status::{ResourceStatus, resource_name};
use crate::io2::*;
use crate::record::Record;
use crate::tests::utils::pair;

fn null() -> OwnedFd {
    File::open("/dev/null").unwrap().into()
}

/// Member 0 holds the real resource 0 and the sim resource 1; member 1 holds
/// resource 0.
struct Rig {
    d: Dispatch,
    fd: i32,
}

fn rig() -> Rig {
    let arena = || Rc::new(Arena::owning(vec![0; 64].into()));
    let fd = null();
    let raw = fd.as_raw_fd();
    let resources = vec![
        Resource {
            name: resource_name("tty"),
            backed: Backed::Fd(fd, Class::Tty),
        },
        Resource {
            name: resource_name("sim"),
            backed: Backed::Sim(4),
        },
    ];
    let members = vec![(arena(), vec![0, 1]), (arena(), vec![0])];
    Rig {
        d: Dispatch::new(members, resources).unwrap(),
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
    fn routes(&mut self) -> Vec<Route> {
        self.d.routes().collect()
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
    r.d.lower(1, read(3, TTY));
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
    r.d.lower(0, read(2, SIM));
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
        (
            sqe(MAX_OPS as u16, TTY, Op::Fsync, Span::default()),
            IoError::Refused,
        ),
    ];
    for (sqe, e) in cases {
        r.d.lower(0, sqe);
        assert_eq!(r.replies(), [(0, Cqe::err(sqe.token, e))]);
    }
    let mut bad = sqe(0, TTY, Op::Fsync, Span::default());
    bad.opcode = 0xEE;
    r.d.lower(0, bad);
    assert_eq!(
        r.replies(),
        [(0, Cqe::err(bad.token, IoError::Unsupported))]
    );
    assert!(r.routes().is_empty());
}

#[test]
fn a_live_slot_is_not_reused() {
    let mut r = rig();
    r.d.lower(0, read(1, TTY));
    r.d.lower(0, read(1, TTY));
    assert_eq!(r.routes().len(), 1);
    assert_eq!(
        r.replies(),
        [(0, Cqe::err(Token::new(1, 7), IoError::Refused))]
    );
}

#[test]
fn closed_grant_is_stale_until_a_remake() {
    let mut r = rig();
    r.d.lower(0, sqe(0, TTY, Op::Close, Span::default()));
    assert_eq!(r.replies(), [(0, Cqe::ok(Token::new(0, 7), 0))]);
    assert!(r.routes().is_empty(), "the resource stays open");
    r.d.lower(0, sqe(1, TTY, Op::Fsync, Span::default()));
    assert_eq!(r.replies()[0].1.result(), Err(IoError::Stale));
    r.d.begin(0);
    r.d.drained(0);
    r.d.end(0);
    r.d.lower(0, sqe(1, TTY, Op::Fsync, Span::default()));
    assert_eq!(r.routes().len(), 1);
}

#[test]
fn accepted_stream_is_a_derived_handle() {
    let mut r = rig();
    r.d.lower(0, sqe(0, TTY, Op::Accept, Span::default()));
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
    r.d.lower(0, sqe(1, stream, Op::Recv, Span::new(0, 4)));
    assert_eq!(r.only_os().class, Class::Stream);
    r.d.lower(0, sqe(2, stream, Op::Close, Span::default()));
    assert!(matches!(r.routes()[..], [Route::Close(_)]));
    r.d.lower(0, sqe(3, stream, Op::Fsync, Span::default()));
    assert_eq!(r.replies().last().unwrap().1.result(), Err(IoError::Stale));
}

#[test]
fn full_derived_table_closes_and_refuses() {
    let mut r = rig();
    let accept = |r: &mut Rig, slot: usize| {
        r.d.lower(0, sqe(slot as u16, TTY, Op::Accept, Span::default()));
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
    r.d.lower(0, read(0, TTY));
    r.d.lower(0, read(1, SIM));
    let _ = r.routes();
    let cancel = |slot| sqe(slot, TTY, Op::Cancel, Span::default());
    for slot in [0, 0, 1, 5] {
        r.d.lower(0, cancel(slot));
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
    r.d.lower(0, read(0, TTY));
    r.d.lower(0, sqe(1, TTY, Op::Accept, Span::default()));
    let ids: Vec<OpId> = r
        .routes()
        .into_iter()
        .map(|route| match route {
            Route::Os(op) => op.id,
            other => panic!("{other:?}"),
        })
        .collect();
    r.d.begin(0);
    assert_eq!(r.routes().len(), 2, "one cancel per op");
    r.d.begin(0);
    assert!(r.routes().is_empty(), "never twice");
    r.d.lower(0, read(2, TTY));
    assert!(
        r.routes().is_empty() && r.replies().is_empty(),
        "new ops are dropped"
    );
    assert!(!r.d.idle(0));
    r.d.drained(0);
    assert_eq!(r.done(ids[0], Err(IoError::Canceled)), None);
    assert!(!r.d.idle(0));
    assert_eq!(r.done(ids[1], Ok(Out::Opened(null(), Class::Stream))), None);
    assert!(
        matches!(r.routes()[..], [Route::Close(_)]),
        "nothing will close it"
    );
    assert!(r.d.idle(0));
    assert!(r.d.idle(1), "a member not quiescing is idle");
}

#[test]
fn drain_closes_derived_handles() {
    let mut r = rig();
    r.d.lower(0, sqe(0, TTY, Op::Accept, Span::default()));
    let op = r.only_os();
    r.done(op.id, Ok(Out::Opened(null(), Class::Stream)));
    r.d.begin(0);
    r.d.drained(0);
    r.d.drained(0);
    assert!(matches!(r.routes()[..], [Route::Close(_)]));
}

#[test]
fn report_writes_changed_resources() {
    let mut r = rig();
    r.d.lower(0, read(0, TTY));
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
            r.d.lower(0, read(0, TTY));
            ids.extend(r.d.routes().map(|route| match route {
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
