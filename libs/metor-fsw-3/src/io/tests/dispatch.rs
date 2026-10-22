use core::time::Duration;
use std::fs::File;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, OwnedFd};

use metor_proto::types::Timestamp;
use zerocopy::FromBytes;

use super::allocations;
use crate::io::arena::Arena;
use crate::io::host::dispatch::Rejected;
use crate::io::host::dispatch::Resource;
use crate::io::host::wire::{FIFO, Wire};
use crate::io::host::{Backed, Class, Dispatch, MemberDef, OpId, OsDone, OsOp, Out, Route};
use crate::io::status::ResourceStatus;
use crate::io::*;
use crate::record::Record;
use crate::tests::utils::pair;

const OPS: usize = DEFAULT_OPS as usize;

fn member(ops: u16, grants: Vec<(ResourceIndex, ResourceEnd)>) -> MemberDef {
    MemberDef { ops, grants }
}

fn null() -> OwnedFd {
    File::open("/dev/null").unwrap().into()
}

/// Member 0 holds the real resource 0 and the host end of the sim UART 1;
/// member 1 holds resource 0 and the UART's device end.
struct Rig {
    d: Dispatch,
    arenas: [Arena; 2],
    fd: i32,
}

fn rig() -> Rig {
    rig_with(SimKind::Uart)
}

/// The rig, with a sim resource of `kind`.
fn rig_with(kind: SimKind) -> Rig {
    let fd = null();
    let raw = fd.as_raw_fd();
    let resources = vec![
        Resource {
            id: "tty".into(),
            backed: Backed::Fd(fd, Class::Tty),
            allow: Box::default(),
        },
        Resource {
            id: "sim".into(),
            backed: Backed::Wire(0),
            allow: Box::default(),
        },
    ];
    let members = vec![
        member(
            DEFAULT_OPS,
            vec![(0.into(), ResourceEnd::Host), (1.into(), ResourceEnd::Host)],
        ),
        member(
            DEFAULT_OPS,
            vec![
                (0.into(), ResourceEnd::Host),
                (1.into(), ResourceEnd::Device),
            ],
        ),
    ];
    Rig {
        d: Dispatch::new(members, resources, vec![Wire::new(kind, 2 * OPS)]).unwrap(),
        arenas: [super::arena(64), super::arena(64)],
        fd: raw,
    }
}

const TTY: RawHandle = RawHandle::new(0, 0);
const SIM: RawHandle = RawHandle::new(1, 0);

fn sqe(slot: u16, handle: RawHandle, op: Op, buf: Span) -> Sqe {
    Sqe::new(Token::new(slot, 7), handle, &op, buf)
}

/// A `Stream` on resource 0, its peer written at the arena's start.
fn stream(slot: u16) -> Sqe {
    sqe(slot, TTY, Op::Stream, Span::new(0, RawAddr::LEN as u32))
}

/// A new stream from `peer`.
fn opened(peer: Option<&str>) -> Result<Out, IoError> {
    Ok(Out::Opened {
        fd: null(),
        class: Class::Stream,
        peer: peer.map(|p| p.parse().unwrap()),
    })
}

fn read(slot: u16, handle: RawHandle) -> Sqe {
    sqe(slot, handle, Op::Read { offset: None }, Span::new(8, 16))
}

impl Rig {
    fn lower(&mut self, m: usize, sqe: Sqe) {
        assert_eq!(self.d.lower(m, sqe, &self.arenas[m], Timestamp(0)), Ok(()));
    }

    fn routes(&mut self) -> Vec<Route> {
        self.d.drain_routes().collect()
    }

    fn replies(&mut self) -> Vec<(usize, Cqe)> {
        self.posted(|_| true)
    }

    /// Posts the completions of the members `owns` picks, and returns them.
    fn posted(&mut self, owns: impl Fn(usize) -> bool) -> Vec<(usize, Cqe)> {
        let mut out = Vec::new();
        self.d.post_ready(0, |m, cqe| {
            owns(m).then(|| out.push((m, *cqe)))?;
            Some(true)
        });
        out
    }

    fn only_os(&mut self) -> OsOp {
        let mut routes = self.routes();
        match (routes.pop(), routes.is_empty()) {
            (Some(Route::Os(op)), true) => op,
            other => panic!("{other:?}"),
        }
    }

    fn done(&mut self, id: OpId, result: Result<Out, IoError>) -> Option<(usize, Cqe)> {
        self.d.raise(OsDone { id, result });
        self.replies().pop()
    }

    /// Fills member `m`'s bytes at `span`.
    fn fill(&self, m: usize, span: Span, bytes: &[u8]) {
        // SAFETY: nothing else touches the test's arenas.
        let buf = unsafe { self.arenas[m].get_slice(span).unwrap().as_mut() };
        buf[..bytes.len()].copy_from_slice(bytes);
    }

    /// Every resource's changed status.
    fn status(&mut self) -> Vec<ResourceStatus> {
        let (_ring, mut out, mut inp) = pair::<ResourceStatus>(ResourceStatus::DEPTH);
        self.d.report(Timestamp(1), &mut out);
        inp.drain().map(|s| *s.unwrap()).collect()
    }

    fn bytes(&self, m: usize, span: Span) -> Vec<u8> {
        // SAFETY: as in `fill`.
        unsafe { self.arenas[m].get_slice(span).unwrap().as_ref() }.to_vec()
    }
}

#[test]
fn real_op_resolves_its_descriptor_buffer_and_index() {
    let mut r = rig();
    r.lower(1, read(3, TTY));
    let op = r.only_os();
    assert_eq!((op.fd, op.class), (r.fd, Class::Tty));
    assert_eq!(op.id, OpId::new((OPS + 3) as u16, 7));
    assert_eq!(op.buf.unwrap().len(), 16);
    let done = r.done(op.id, Ok(Out::Len(5)));
    assert_eq!(done, Some((1, Cqe::ok(Token::new(3, 7), 5))));
    assert_eq!(r.done(op.id, Ok(Out::Len(5))), None, "raised once");
}

#[test]
fn a_write_reaches_a_read_parked_on_the_other_end() {
    let mut r = rig();
    r.lower(1, read(2, SIM));
    assert!(r.replies().is_empty(), "nothing to read yet");
    let span = Span::new(0, 5);
    r.fill(0, span, b"$PMTK");
    r.lower(0, sqe(4, SIM, Op::Write { offset: None }, span));
    let got = r.replies();
    let wrote = (0, Cqe::ok(Token::new(4, 7), 5));
    let read = (1, Cqe::ok(Token::new(2, 7), 5));
    assert_eq!(got, [read, wrote], "the woken read lands first");
    assert_eq!(&r.bytes(1, Span::new(8, 5)), b"$PMTK");
    assert!(r.routes().is_empty(), "a wire op never reaches the backend");
}

#[test]
fn a_read_takes_what_the_fifo_holds() {
    let mut r = rig();
    r.fill(1, Span::new(0, 3), b"abc");
    r.lower(1, sqe(0, SIM, Op::Write { offset: None }, Span::new(0, 3)));
    r.lower(0, sqe(1, SIM, Op::Read { offset: None }, Span::new(8, 2)));
    r.lower(0, sqe(2, SIM, Op::Read { offset: None }, Span::new(16, 8)));
    let got: Vec<_> = r
        .replies()
        .into_iter()
        .map(|(m, c)| (m, c.result()))
        .collect();
    assert_eq!(got, [(1, Ok(3)), (0, Ok(2)), (0, Ok(1))]);
    assert_eq!(&r.bytes(0, Span::new(8, 2)), b"ab");
    assert_eq!(&r.bytes(0, Span::new(16, 1)), b"c");
}

#[test]
fn a_full_fifo_drops_what_does_not_fit() {
    let mut r = rig();
    let span = Span::new(0, 64);
    for k in 0..(FIFO / 64 + 1) {
        let slot = (k % OPS) as u16;
        r.lower(0, sqe(slot, SIM, Op::Write { offset: None }, span));
        assert_eq!(
            r.replies()[0].1.result(),
            Ok(64),
            "the line takes the write whole"
        );
    }
    let mut total = 0;
    for slot in 0..(FIFO / 64 + 1) as u16 {
        r.lower(
            1,
            sqe(slot % DEFAULT_OPS, SIM, Op::Read { offset: None }, span),
        );
        match r.replies().pop() {
            Some((_, c)) => total += c.result().unwrap(),
            None => break,
        }
    }
    assert_eq!(total as usize, FIFO);
    let sim = r.status().pop().map(|s| s.dropped);
    assert_eq!(sim, Some(64), "one write past the FIFO");
}

#[test]
fn a_flush_empties_only_the_flushing_ends_fifo() {
    let mut r = rig();
    r.fill(1, Span::new(0, 3), b"abc");
    r.lower(1, sqe(0, SIM, Op::Write { offset: None }, Span::new(0, 3)));
    r.lower(
        0,
        sqe(1, SIM, Op::TtyFlush(TtyQueue::Output), Span::default()),
    );
    r.lower(
        0,
        sqe(2, SIM, Op::TtyFlush(TtyQueue::Input), Span::default()),
    );
    assert_eq!(r.replies().len(), 3);
    r.lower(0, read(3, SIM));
    assert!(
        r.replies().is_empty(),
        "the flush emptied it, so the read parks"
    );
}

#[test]
fn a_read_of_no_bytes_finishes_at_once() {
    let mut r = rig();
    r.lower(0, sqe(0, SIM, Op::Read { offset: None }, Span::new(0, 0)));
    assert_eq!(r.replies(), [(0, Cqe::ok(Token::new(0, 7), 0))]);
}

#[test]
fn a_wire_refuses_ops_its_kind_lacks() {
    let mut r = rig();
    r.lower(0, sqe(0, SIM, Op::Fsync, Span::default()));
    let mut g = rig_with(SimKind::GpioLine);
    g.lower(1, read(0, SIM));
    let unsupported = |m| [(m, Cqe::err(Token::new(0, 7), IoError::Unsupported))];
    assert_eq!(r.replies(), unsupported(0));
    assert_eq!(g.replies(), unsupported(1));
}

#[test]
fn a_cancel_on_the_device_end_completes_its_read() {
    let mut r = rig();
    r.lower(1, read(4, SIM));
    r.lower(1, sqe(4, SIM, Op::Cancel, Span::default()));
    assert_eq!(
        r.replies(),
        [(1, Cqe::err(Token::new(4, 7), IoError::Canceled))]
    );
    assert!(r.routes().is_empty());
}

#[test]
fn a_quiescing_model_with_a_parked_read_goes_idle_once_drained() {
    let mut r = rig();
    r.lower(1, read(0, SIM));
    r.d.quiesce_member(1);
    assert!(
        r.replies().is_empty(),
        "a quiescing member gets no completion"
    );
    assert!(!r.d.member_idle(1), "its queue may still hold ops");
    r.d.member_drained(1);
    assert!(r.d.member_idle(1));
}

#[test]
fn each_io_step_takes_only_the_completions_it_posts() {
    let mut r = rig();
    r.lower(0, sqe(0, SIM, Op::Write { offset: None }, Span::new(0, 1)));
    r.lower(1, sqe(1, RawHandle::new(9, 0), Op::Fsync, Span::default()));
    let first = r.posted(|m| m == 1);
    let second = r.posted(|m| m == 0);
    assert_eq!(first, [(1, Cqe::err(Token::new(1, 7), IoError::Stale))]);
    assert_eq!(second, [(0, Cqe::ok(Token::new(0, 7), 1))]);
    assert!(r.replies().is_empty());
}

#[test]
fn a_refused_completion_waits_in_the_dispatch() {
    let stale = || (1, Cqe::err(Token::new(1, 7), IoError::Stale));
    let mut r = rig();
    r.lower(1, sqe(1, RawHandle::new(9, 0), Op::Fsync, Span::default()));
    assert!(!r.d.post_ready(1, |_, _| Some(false)), "held, not lost");
    assert_eq!(r.replies(), [stale()]);
    r.lower(1, sqe(1, RawHandle::new(9, 0), Op::Fsync, Span::default()));
    assert!(r.d.post_ready(0, |_, _| Some(false)), "lost past the hold");
    assert!(r.replies().is_empty());
}

#[test]
fn gpio_level_is_shared_by_both_ends() {
    let mut r = rig_with(SimKind::GpioLine);
    r.lower(1, sqe(0, SIM, Op::GpioSet(true), Span::default()));
    r.lower(0, sqe(0, SIM, Op::GpioGet, Span::default()));
    let got: Vec<_> = r
        .replies()
        .into_iter()
        .map(|(m, c)| (m, c.result()))
        .collect();
    assert_eq!(got, [(1, Ok(0)), (0, Ok(1))]);
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
        (
            sqe(
                0,
                RawHandle::new(9, 0),
                Op::Timeout(Duration::ZERO),
                Span::default(),
            ),
            IoError::Stale,
        ),
        (sqe(0, TTY, Op::Recv, Span::new(60, 8)), IoError::BadBuf),
        (
            sqe(0, SIM, Op::Stream, Span::new(0, RawAddr::LEN as u32)),
            IoError::Unsupported,
        ),
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
    let past = sqe(DEFAULT_OPS, TTY, Op::Fsync, Span::default());
    for bad in [read(1, TTY), reused, past] {
        assert_eq!(r.d.lower(0, bad, &r.arenas[0], Timestamp(0)), Err(Rejected));
    }
    assert!(r.routes().is_empty() && r.replies().is_empty());
}

#[test]
fn teardown_waits_only_for_ops_on_descriptors() {
    let mut r = rig();
    r.lower(0, read(0, SIM));
    let _ = r.routes();
    assert!(!r.d.has_kernel_ops_in_flight(), "a model holds the op");
    r.lower(0, read(1, TTY));
    assert!(r.d.has_kernel_ops_in_flight());
}

#[test]
fn closed_grant_is_stale_until_a_remake() {
    let mut r = rig();
    r.lower(0, sqe(0, TTY, Op::Close, Span::default()));
    assert_eq!(r.replies(), [(0, Cqe::ok(Token::new(0, 7), 0))]);
    assert!(r.routes().is_empty(), "the resource stays open");
    for op in [Op::Fsync, Op::Timeout(Duration::ZERO)] {
        r.lower(0, sqe(1, TTY, op, Span::default()));
        assert_eq!(r.replies()[0].1.result(), Err(IoError::Stale), "{op:?}");
    }
    r.d.quiesce_member(0);
    r.d.member_drained(0);
    r.d.end_quiescing_member(0);
    r.lower(0, sqe(1, TTY, Op::Fsync, Span::default()));
    assert_eq!(r.routes().len(), 1);
}

#[test]
fn accepted_stream_is_a_derived_handle() {
    let mut r = rig();
    r.lower(0, stream(0));
    let op = r.only_os();
    let (_, cqe) = r.done(op.id, opened(None)).unwrap();
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
        r.lower(0, stream(slot as u16));
        let op = r.only_os();
        r.done(op.id, opened(None)).unwrap().1
    };
    for slot in 0..OPS {
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
    for slot in [0, 0, 1, 1, 5] {
        r.lower(0, cancel(slot));
    }
    let routes = r.routes();
    assert!(matches!(routes[..], [Route::Cancel(id)] if id == OpId::new(0, 7)));
    let got = r.replies();
    assert_eq!(
        got,
        [(0, Cqe::err(Token::new(1, 7), IoError::Canceled))],
        "a parked read completes at once"
    );
}

#[test]
fn quiesce_cancels_drops_and_waits() {
    let mut r = rig();
    r.lower(0, read(0, TTY));
    r.lower(0, stream(1));
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
    assert_eq!(r.done(ids[1], opened(None)), None);
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
    r.lower(0, stream(0));
    let op = r.only_os();
    r.done(op.id, opened(None));
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
    let (_ring, mut out, mut inp) = pair::<ResourceStatus>(ResourceStatus::DEPTH);
    r.d.report(Timestamp(1), &mut out);
    let got: Vec<ResourceStatus> = inp.drain().map(|s| *s.unwrap()).collect();
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].submitted, got[0].completed, got[0].bytes_in),
        (1, 1, 4)
    );
    r.d.report(Timestamp(2), &mut out);
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
            r.d.post_ready(0, |_, _| Some(true));
        }
    });
    assert_eq!(n, 0);
}

/// Members of budgets `ops` on one real resource.
fn budgets(ops: &[u16]) -> Result<Dispatch, crate::coordinator::BuildError> {
    let resources = vec![Resource {
        id: "tty".into(),
        backed: Backed::Fd(null(), Class::Tty),
        allow: Box::default(),
    }];
    let grants = || vec![(0.into(), ResourceEnd::Host)];
    let members = ops.iter().map(|&n| member(n, grants())).collect();
    Dispatch::new(members, resources, Vec::new())
}

/// Lowers an fsync into every slot of every member, then finishes each op:
/// every completion reaches the member and slot that submitted it.
fn fill_every_slot(ops: &[u16]) {
    let mut d = budgets(ops).unwrap();
    let arena = super::arena(0);
    for (m, &n) in ops.iter().enumerate() {
        for slot in 0..n {
            let fsync = sqe(slot, TTY, Op::Fsync, Span::default());
            assert_eq!(d.lower(m, fsync, &arena, Timestamp(0)), Ok(()));
        }
    }
    let ids: Vec<_> = d
        .drain_routes()
        .map(|r| match r {
            Route::Os(op) => op.id,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        ids.len(),
        ops.iter().map(|&n| usize::from(n)).sum::<usize>()
    );
    for id in ids {
        d.raise(OsDone {
            id,
            result: Ok(Out::Len(0)),
        });
    }
    let mut got = Vec::new();
    d.post_ready(0, |m, cqe| {
        got.push((m, cqe.token.index()));
        Some(true)
    });
    let want: Vec<_> = ops
        .iter()
        .enumerate()
        .flat_map(|(m, &n)| (0..n).map(move |slot| (m, slot)))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn each_member_fills_its_own_budget() {
    for ops in [&[16, 40][..], &[1, 3, 1], &[0, 2]] {
        fill_every_slot(ops);
    }
}

#[test]
fn a_slot_past_the_members_budget_is_rejected() {
    let mut d = budgets(&[2, 40]).unwrap();
    let arena = super::arena(0);
    let past = sqe(2, TTY, Op::Fsync, Span::default());
    assert_eq!(d.lower(0, past, &arena, Timestamp(0)), Err(Rejected));
    assert_eq!(d.drain_routes().count(), 0);
    assert_eq!(d.lower(1, past, &arena, Timestamp(0)), Ok(()));
}

#[test]
fn budgets_past_the_op_id_space_fail_the_build() {
    let fits = [u16::MAX, 1];
    assert_eq!(budgets(&fits).map(|d| d.ops()).ok(), Some(1 << 16));
    let past = [u16::MAX, 2];
    assert!(matches!(
        budgets(&past),
        Err(crate::coordinator::BuildError::TooManyIoOps { ops: 65537 })
    ));
}

#[test]
fn held_scales_with_the_budgets_posted() {
    let d = budgets(&[4, 40]).unwrap();
    assert_eq!(d.held_for(&[0, 1]), 2 * 44);
    assert_eq!(d.held_for(&[]), 0);
}

/// Member 0 holds a listener that admits the peers `allow` names, and a
/// dialer of `10.0.0.9:2240`.
fn endpoints(allow: &[&str]) -> (Dispatch, Arena) {
    let resources = vec![
        Resource {
            id: "listen".into(),
            backed: Backed::Fd(null(), Class::Listener),
            allow: allow.iter().map(|c| c.parse().unwrap()).collect(),
        },
        Resource {
            id: "dial".into(),
            backed: Backed::Dial("10.0.0.9:2240".parse().unwrap()),
            allow: Box::default(),
        },
    ];
    let grants = vec![(0.into(), ResourceEnd::Host), (1.into(), ResourceEnd::Host)];
    let d = Dispatch::new(vec![member(DEFAULT_OPS, grants)], resources, Vec::new());
    (d.unwrap(), super::arena(64))
}

const DIAL: RawHandle = RawHandle::new(1, 0);

fn os_ops(d: &mut Dispatch) -> Vec<OsOp> {
    d.drain_routes()
        .filter_map(|r| match r {
            Route::Os(op) => Some(op),
            _ => None,
        })
        .collect()
}

fn ready(d: &mut Dispatch) -> Vec<Cqe> {
    let mut got = Vec::new();
    d.post_ready(0, |_, cqe| {
        got.push(*cqe);
        Some(true)
    });
    got
}

fn peer_in(arena: &Arena) -> SocketAddr {
    // SAFETY: nothing else touches the test's arena.
    let bytes = unsafe {
        arena
            .get_slice(Span::new(0, RawAddr::LEN as u32))
            .unwrap()
            .as_ref()
    };
    SocketAddr::try_from(RawAddr::read_from_bytes(bytes).unwrap()).unwrap()
}

fn rejected(d: &mut Dispatch) -> u64 {
    let (_ring, mut out, mut inp) = pair::<ResourceStatus>(ResourceStatus::DEPTH);
    d.report(Timestamp(1), &mut out);
    let status = inp
        .drain()
        .map(|s| *s.unwrap())
        .find(|s| s.resource.to_str() == Ok("listen"));
    status.map_or(0, |s| s.rejected)
}

#[test]
fn an_accepted_peer_lands_in_the_buffer() {
    let (mut d, arena) = endpoints(&[]);
    assert_eq!(d.lower(0, stream(0), &arena, Timestamp(0)), Ok(()));
    let [op] = os_ops(&mut d)[..] else { panic!() };
    assert_eq!((op.class, op.dial), (Class::Listener, None));
    d.raise(OsDone {
        id: op.id,
        result: opened(Some("10.0.0.7:5000")),
    });
    assert!(ready(&mut d)[0].result().is_ok());
    assert_eq!(peer_in(&arena), "10.0.0.7:5000".parse().unwrap());
}

#[test]
fn a_dial_lowers_with_its_address_and_no_descriptor() {
    let (mut d, arena) = endpoints(&[]);
    let dial = sqe(0, DIAL, Op::Stream, Span::new(0, RawAddr::LEN as u32));
    assert_eq!(d.lower(0, dial, &arena, Timestamp(0)), Ok(()));
    let [op] = os_ops(&mut d)[..] else { panic!() };
    assert_eq!((op.fd, op.class), (-1, Class::Dialer));
    assert_eq!(op.dial, Some("10.0.0.9:2240".parse().unwrap()));
    d.raise(OsDone {
        id: op.id,
        result: opened(Some("10.0.0.9:2240")),
    });
    assert!(ready(&mut d)[0].result().is_ok());
    assert_eq!(peer_in(&arena), "10.0.0.9:2240".parse().unwrap());
}

#[test]
fn a_dialer_takes_only_streams() {
    let (mut d, arena) = endpoints(&[]);
    assert_eq!(d.lower(0, read(0, DIAL), &arena, Timestamp(0)), Ok(()));
    assert!(os_ops(&mut d).is_empty());
    assert_eq!(ready(&mut d)[0].result(), Err(IoError::Unsupported));
}

#[test]
fn a_rejected_peer_is_closed_and_the_op_runs_again() {
    let (mut d, arena) = endpoints(&["10.0.0.0/24"]);
    d.lower(0, stream(0), &arena, Timestamp(0)).unwrap();
    let [op] = os_ops(&mut d)[..] else { panic!() };
    d.raise(OsDone {
        id: op.id,
        result: opened(Some("192.168.1.7:5000")),
    });
    assert!(ready(&mut d).is_empty(), "the system never sees it");
    let routes: Vec<_> = d.drain_routes().collect();
    assert!(matches!(&routes[..], [Route::Close(_), Route::Os(again)] if again.id == op.id));
    assert_eq!(rejected(&mut d), 1);
    d.raise(OsDone {
        id: op.id,
        result: opened(Some("10.0.0.3:6000")),
    });
    let [cqe] = ready(&mut d)[..] else { panic!() };
    assert_eq!(cqe.token, Token::new(0, 7));
    assert!(cqe.result().is_ok());
    assert_eq!(peer_in(&arena), "10.0.0.3:6000".parse().unwrap());
}

#[test]
fn an_allow_list_refuses_a_stream_with_no_peer() {
    let (mut d, arena) = endpoints(&["0.0.0.0/0"]);
    d.lower(0, stream(0), &arena, Timestamp(0)).unwrap();
    let [op] = os_ops(&mut d)[..] else { panic!() };
    d.raise(OsDone {
        id: op.id,
        result: opened(None),
    });
    assert!(ready(&mut d).is_empty());
    assert_eq!(rejected(&mut d), 1);
}

#[test]
fn a_canceled_or_quiescing_op_is_not_run_again() {
    let (mut d, arena) = endpoints(&["10.0.0.0/24"]);
    d.lower(0, stream(0), &arena, Timestamp(0)).unwrap();
    let [op] = os_ops(&mut d)[..] else { panic!() };
    d.lower(
        0,
        sqe(0, TTY, Op::Cancel, Span::default()),
        &arena,
        Timestamp(0),
    )
    .unwrap();
    let _ = d.drain_routes();
    d.raise(OsDone {
        id: op.id,
        result: opened(Some("192.168.1.7:5000")),
    });
    assert!(matches!(
        d.drain_routes().collect::<Vec<_>>()[..],
        [Route::Close(_)]
    ));
    assert_eq!(ready(&mut d)[0].result(), Err(IoError::Canceled));

    d.lower(0, stream(1), &arena, Timestamp(0)).unwrap();
    let [op] = os_ops(&mut d)[..] else { panic!() };
    d.quiesce_member(0);
    let _ = d.drain_routes();
    d.raise(OsDone {
        id: op.id,
        result: opened(Some("192.168.1.7:5000")),
    });
    assert!(matches!(
        d.drain_routes().collect::<Vec<_>>()[..],
        [Route::Close(_)]
    ));
    assert!(ready(&mut d).is_empty());
    assert!(!d.has_kernel_ops_in_flight());
}

#[test]
fn a_short_peer_buffer_is_bad_buf() {
    let (mut d, arena) = endpoints(&[]);
    let short = sqe(0, TTY, Op::Stream, Span::new(0, RawAddr::LEN as u32 - 1));
    assert_eq!(d.lower(0, short, &arena, Timestamp(0)), Ok(()));
    assert!(os_ops(&mut d).is_empty());
    assert_eq!(ready(&mut d)[0].result(), Err(IoError::BadBuf));
}

fn timeout(slot: u16, micros: u64) -> Sqe {
    let after = Duration::from_micros(micros);
    sqe(slot, TTY, Op::Timeout(after), Span::default())
}

#[test]
fn a_timeout_lands_at_its_deadline_and_never_reaches_the_os() {
    let mut r = rig();
    r.d.lower(0, timeout(2, 30), &r.arenas[0], Timestamp(100))
        .unwrap();
    assert!(r.routes().is_empty());
    r.d.expire(Timestamp(129));
    assert!(r.replies().is_empty());
    r.d.expire(Timestamp(130));
    assert_eq!(r.replies(), [(0, Cqe::ok(Token::new(2, 7), 0))]);
    r.d.expire(Timestamp(1_000));
    assert!(r.replies().is_empty(), "lands once");
}

#[test]
fn zero_and_equal_timeouts_land_together() {
    let mut r = rig();
    for (m, slot, micros) in [(0, 0, 0), (0, 1, 5), (1, 0, 5)] {
        r.d.lower(m, timeout(slot, micros), &r.arenas[m], Timestamp(10))
            .unwrap();
    }
    r.d.expire(Timestamp(10));
    assert_eq!(r.replies(), [(0, Cqe::ok(Token::new(0, 7), 0))]);
    r.d.expire(Timestamp(15));
    let got = r.replies();
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|(_, c)| c.result() == Ok(0)));
}

#[test]
fn a_canceled_timeout_lands_canceled_and_never_ok() {
    let mut r = rig();
    r.d.lower(0, timeout(1, 30), &r.arenas[0], Timestamp(0))
        .unwrap();
    r.lower(0, sqe(1, TTY, Op::Cancel, Span::default()));
    assert!(r.routes().is_empty(), "nothing to cancel in the backend");
    assert_eq!(
        r.replies(),
        [(0, Cqe::err(Token::new(1, 7), IoError::Canceled))]
    );
    r.d.expire(Timestamp(1_000));
    assert!(r.replies().is_empty());
    assert_eq!(
        r.d.lower(0, timeout(1, 30), &r.arenas[0], Timestamp(0)),
        Ok(()),
        "the slot is free"
    );
}

#[test]
fn quiesce_clears_a_members_timers() {
    let mut r = rig();
    r.d.lower(0, timeout(0, 30), &r.arenas[0], Timestamp(0))
        .unwrap();
    assert!(
        !r.d.has_kernel_ops_in_flight(),
        "a timer is not the kernel's"
    );
    r.d.quiesce_member(0);
    r.d.member_drained(0);
    assert!(r.d.member_idle(0));
    r.d.expire(Timestamp(1_000));
    assert!(r.replies().is_empty());
}

#[test]
fn the_longest_timeout_lands_at_the_end_of_time() {
    let mut r = rig();
    let forever = sqe(0, TTY, Op::Timeout(Duration::MAX), Span::default());
    r.d.lower(0, forever, &r.arenas[0], Timestamp(10)).unwrap();
    r.d.expire(Timestamp(i64::MAX - 1));
    assert!(r.replies().is_empty());
    r.d.expire(Timestamp(i64::MAX));
    assert_eq!(r.replies(), [(0, Cqe::ok(Token::new(0, 7), 0))]);
}

#[test]
fn a_rearmed_slot_lands_once_for_its_new_generation() {
    let mut r = rig();
    let at = |generation, micros| {
        let after = Duration::from_micros(micros);
        Sqe::new(
            Token::new(1, generation),
            TTY,
            &Op::Timeout(after),
            Span::default(),
        )
    };
    r.d.lower(0, at(7, 10), &r.arenas[0], Timestamp(0)).unwrap();
    r.lower(0, sqe(1, TTY, Op::Cancel, Span::default()));
    r.d.lower(0, at(8, 30), &r.arenas[0], Timestamp(0)).unwrap();
    assert_eq!(
        r.replies(),
        [(0, Cqe::err(Token::new(1, 7), IoError::Canceled))]
    );
    r.d.expire(Timestamp(10));
    assert!(r.replies().is_empty(), "the canceled deadline is gone");
    r.d.expire(Timestamp(30));
    assert_eq!(r.replies(), [(0, Cqe::ok(Token::new(1, 8), 0))]);
}

/// A re-lowered accept still waiting for the backend when teardown starts
/// is completed, so its slot frees and teardown ends.
#[test]
fn teardown_completes_an_op_waiting_to_be_lowered() {
    let (mut d, arena) = endpoints(&["10.0.0.0/24"]);
    d.lower(0, stream(0), &arena, Timestamp(0)).unwrap();
    let [op] = os_ops(&mut d)[..] else { panic!() };
    d.raise(OsDone {
        id: op.id,
        result: opened(Some("192.168.1.7:5000")),
    });
    let mut host = crate::io::host::Host {
        dispatch: d,
        os: crate::io::host::host(OPS).unwrap(),
        queues: Box::default(),
    };
    assert!(host.teardown());
    assert!(!host.dispatch.has_kernel_ops_in_flight());
}
