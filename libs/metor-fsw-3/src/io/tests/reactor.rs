use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use super::allocations;
use crate::io::*;

/// The ops a `ScriptOs` received and the completions it returns next.
#[derive(Default)]
struct Script {
    pushed: Vec<Sqe>,
    submits: usize,
    answers: Vec<Cqe>,
    registered: u32,
}

/// A fake `Os` that records ops and returns the completions a test queues.
struct ScriptOs(Rc<RefCell<Script>>);

impl Os for ScriptOs {
    fn push(&mut self, sqe: Sqe) {
        self.0.borrow_mut().pushed.push(sqe);
    }

    fn submit(&mut self) {
        self.0.borrow_mut().submits += 1;
    }

    fn reap(&mut self, done: &mut Vec<Cqe>) {
        done.append(&mut self.0.borrow_mut().answers);
    }

    fn register(&mut self, _fd: OwnedFd, _class: FdClass) -> RawHandle {
        let mut s = self.0.borrow_mut();
        s.registered += 1;
        RawHandle::new(s.registered - 1, 0)
    }
}

struct Rig {
    reactor: Reactor,
    script: Rc<RefCell<Script>>,
    tables: Vec<Rc<BufTable>>,
    done: Vec<(SourceId, Cqe)>,
}

impl Rig {
    fn new(ops: usize, sources: usize) -> Self {
        let script = Rc::new(RefCell::new(Script::default()));
        let os = Box::new(ScriptOs(script.clone()));
        let mut reactor = Reactor::new(os, Limits { ops, sources });
        let shared = Tables::new();
        let mut tables = Vec::new();
        for _ in 0..sources {
            let t = Rc::new(BufTable::new(4));
            shared.add(&t);
            t.alloc(8).unwrap();
            t.freeze();
            reactor.source(t.clone());
            tables.push(t);
        }
        Self {
            reactor,
            script,
            tables,
            done: Vec::with_capacity(4 * ops),
        }
    }

    fn push(&mut self, src: u16, token: u32, op: Op) {
        let sqe = Sqe::new(Token::new(token), RawHandle::new(0, 0), op);
        self.reactor.push(SourceId(src), sqe);
    }

    fn pushed(&self) -> Vec<Sqe> {
        self.script.borrow().pushed.clone()
    }

    /// Queues a completion for the `i`th op the `Os` received.
    fn answer(&self, i: usize, out: Out) {
        let mut s = self.script.borrow_mut();
        let token = s.pushed[i].token;
        s.answers.push(Cqe::ok(token, out));
    }

    fn reap(&mut self) -> Vec<(SourceId, Cqe)> {
        self.reactor.reap(&mut self.done);
        core::mem::take(&mut self.done)
    }
}

#[test]
fn op_reaches_os_stamped_and_returns_mapped() {
    let mut rig = Rig::new(8, 2);
    rig.push(1, 77, Op::recv(BufRef::new(0)));
    rig.reactor.submit();
    let sent = rig.pushed()[0];
    assert_ne!(sent.token, Token::new(77));
    assert_eq!(sent.op.buf().unwrap().table(), rig.tables[1].id().unwrap());
    rig.answer(0, Out::len(3));
    let done = rig.reap();
    assert_eq!(done, [(SourceId(1), Cqe::ok(Token::new(77), Out::len(3)))]);
}

#[test]
fn same_token_from_two_sources_does_not_collide() {
    let mut rig = Rig::new(8, 2);
    rig.push(0, 5, Op::CLOSE);
    rig.push(1, 5, Op::FSYNC);
    rig.reactor.submit();
    let pushed = rig.pushed();
    assert_ne!(pushed[0].token, pushed[1].token);
    rig.answer(1, Out::len(1));
    rig.answer(0, Out::len(0));
    let done = rig.reap();
    assert_eq!(done[0], (SourceId(1), Cqe::ok(Token::new(5), Out::len(1))));
    assert_eq!(done[1], (SourceId(0), Cqe::ok(Token::new(5), Out::len(0))));
}

#[test]
fn ops_reach_os_only_at_submit() {
    let mut rig = Rig::new(8, 1);
    rig.push(0, 1, Op::CLOSE);
    assert!(rig.pushed().is_empty());
    rig.reactor.submit();
    assert_eq!(rig.pushed().len(), 1);
    assert_eq!(rig.script.borrow().submits, 1);
    assert!(rig.reap().is_empty());
    rig.answer(0, Out::UNIT);
    assert_eq!(rig.reap().len(), 1);
}

#[test]
fn cancel_names_target_reactor_token() {
    let mut rig = Rig::new(8, 1);
    rig.push(0, 3, Op::recv(BufRef::new(0)));
    rig.push(0, 4, Op::cancel(Token::new(3)));
    rig.reactor.submit();
    let pushed = rig.pushed();
    assert_eq!(pushed[1].op, Op::cancel(pushed[0].token));
}

#[test]
fn token_is_reusable_after_completion() {
    let mut rig = Rig::new(1, 1);
    rig.push(0, 1, Op::CLOSE);
    rig.reactor.submit();
    rig.answer(0, Out::UNIT);
    assert_eq!(rig.reap().len(), 1);
    rig.push(0, 2, Op::CLOSE);
    rig.reactor.submit();
    assert_eq!(rig.pushed().len(), 2);
    assert_eq!(rig.pushed()[1].token, rig.pushed()[0].token);
}

#[test]
fn cancel_all_cancels_one_source_and_drops_own_completions() {
    let mut rig = Rig::new(8, 2);
    rig.push(0, 1, Op::CLOSE);
    rig.push(0, 2, Op::CLOSE);
    rig.push(1, 1, Op::CLOSE);
    rig.reactor.submit();
    rig.reactor.cancel_all(SourceId(0));
    rig.reactor.submit();
    let pushed = rig.pushed();
    let cancels: Vec<_> = pushed[3..].iter().map(|s| s.op).collect();
    assert_eq!(
        cancels,
        [Op::cancel(pushed[0].token), Op::cancel(pushed[1].token)]
    );
    for i in 0..pushed.len() {
        rig.answer(i, Out::UNIT);
    }
    assert_eq!(rig.reactor.in_flight(SourceId(0)), 2);
    let done = rig.reap();
    assert_eq!(done.len(), 3);
    assert_eq!(rig.reactor.in_flight(SourceId(0)), 0);
    assert_eq!(rig.reactor.in_flight(SourceId(1)), 0);
}

#[test]
fn own_ops_pass_when_system_tokens_are_full() {
    let mut rig = Rig::new(2, 1);
    rig.push(0, 1, Op::CLOSE);
    rig.push(0, 2, Op::CLOSE);
    rig.reactor.cancel_all(SourceId(0));
    rig.reactor
        .push_own(Sqe::new(Token::new(0), RawHandle::new(9, 0), Op::CLOSE));
    rig.reactor.submit();
    let pushed = rig.pushed();
    assert_eq!(pushed.len(), 5);
    assert!(pushed[2..].iter().all(|s| s.token.get() & 0x8000_0000 != 0));
    for i in 2..5 {
        rig.answer(i, Out::UNIT);
    }
    assert!(rig.reap().is_empty());
}

#[test]
fn op_past_limit_is_queue_full() {
    let mut rig = Rig::new(1, 1);
    rig.push(0, 1, Op::CLOSE);
    rig.push(0, 2, Op::CLOSE);
    rig.reactor.submit();
    assert_eq!(rig.pushed().len(), 1);
    let done = rig.reap();
    assert_eq!(
        done,
        [(SourceId(0), Cqe::err(Token::new(2), IoError::QueueFull))]
    );
}

#[test]
fn unknown_source_is_refused() {
    let mut rig = Rig::new(4, 1);
    rig.push(3, 1, Op::CLOSE);
    rig.reactor.submit();
    assert!(rig.pushed().is_empty());
    let done = rig.reap();
    assert_eq!(
        done,
        [(SourceId(3), Cqe::err(Token::new(1), IoError::Refused))]
    );
}

#[test]
fn cancel_of_nothing_is_enoent() {
    let mut rig = Rig::new(4, 1);
    rig.push(0, 1, Op::cancel(Token::new(9)));
    let done = rig.reap();
    let e = IoError::Os(Errno::ENOENT);
    assert_eq!(done, [(SourceId(0), Cqe::err(Token::new(1), e))]);
}

#[test]
fn unresolved_buf_is_bad_buf() {
    let mut rig = Rig::new(4, 1);
    rig.push(0, 1, Op::recv(BufRef::new(3)));
    let done = rig.reap();
    assert_eq!(
        done,
        [(SourceId(0), Cqe::err(Token::new(1), IoError::BadBuf))]
    );
}

#[test]
fn unknown_token_from_os_is_dropped() {
    let mut rig = Rig::new(4, 1);
    rig.script
        .borrow_mut()
        .answers
        .push(Cqe::ok(Token::new(3), Out::UNIT));
    rig.script
        .borrow_mut()
        .answers
        .push(Cqe::ok(Token::new(0x8000_0001), Out::UNIT));
    assert!(rig.reap().is_empty());
}

/// Also run under Miri, which checks that the leaked table is never freed.
#[test]
fn drop_ends_and_leaks_busy_table() {
    let mut rig = Rig::new(4, 2);
    rig.push(0, 1, Op::recv(BufRef::new(0)));
    rig.reactor.submit();
    let busy = Rc::downgrade(&rig.tables[0]);
    let idle = Rc::downgrade(&rig.tables[1]);
    let script = rig.script.clone();
    drop(rig);
    let s = script.borrow();
    assert_eq!(s.submits, 1 + 3);
    assert_eq!(s.pushed.len(), 2);
    assert!(idle.upgrade().is_none());
    let table = busy.upgrade().expect("busy table leaked");
    // SAFETY: releases the reactor's leaked reference, so Miri sees no leak.
    unsafe { Rc::decrement_strong_count(Rc::as_ptr(&table)) };
}

#[test]
fn steady_cycle_does_not_allocate() {
    let mut rig = Rig::new(64, 1);
    rig.script.borrow_mut().pushed.reserve(1024);
    rig.script.borrow_mut().answers.reserve(64);
    let cycle = |rig: &mut Rig| {
        for t in 0..64 {
            rig.push(0, t, Op::recv(BufRef::new(0)));
        }
        rig.reactor.submit();
        let n = rig.script.borrow().pushed.len();
        for i in n - 64..n {
            rig.answer(i, Out::len(1));
        }
        rig.reactor.reap(&mut rig.done);
        assert_eq!(rig.done.len(), 64);
        rig.done.clear();
    };
    cycle(&mut rig);
    assert_eq!(allocations(|| cycle(&mut rig)), 0);
}

#[test]
fn repeated_cancel_all_cancels_each_op_once() {
    let mut rig = Rig::new(2, 1);
    rig.push(0, 1, Op::CLOSE);
    rig.push(0, 2, Op::CLOSE);
    for _ in 0..4 {
        rig.reactor.cancel_all(SourceId(0));
    }
    rig.reactor.submit();
    let cancels = rig
        .pushed()
        .iter()
        .filter(|s| s.op.kind() == OpKind::Cancel)
        .count();
    assert_eq!(cancels, 2);
}

/// An `IoShared` whose reactor runs over a `ScriptOs`, with one member.
fn quiesce_rig() -> (
    crate::io::IoShared,
    Rc<crate::io::shared::Member>,
    Rc<RefCell<Script>>,
) {
    let mut shared = crate::io::IoShared::default();
    let member = shared.member("gps").unwrap();
    member.table.bufs.freeze();
    let script = Rc::new(RefCell::new(Script::default()));
    let os = Box::new(ScriptOs(script.clone()));
    let mut reactor = Reactor::new(os, Limits { ops: 8, sources: 1 });
    let src = reactor.source(member.table.bufs.clone());
    member.quiesce.src.set(Some(src.0));
    shared.host = Some(crate::io::shared::IoHost { reactor });
    (shared, member, script)
}

fn host(s: &mut crate::io::IoShared) -> &mut Reactor {
    &mut s.host.as_mut().unwrap().reactor
}

#[test]
fn quiesce_waits_for_the_drain_and_the_cancels() {
    let (mut shared, member, script) = quiesce_rig();
    host(&mut shared).push(
        SourceId(0),
        Sqe::new(Token::new(1), RawHandle::new(0, 0), Op::CLOSE),
    );
    host(&mut shared).submit();
    shared.begin(&member);
    assert!(member.quiesce.active.get());
    host(&mut shared).submit();
    let pushed = script.borrow().pushed.clone();
    assert_eq!(pushed[1].op, Op::cancel(pushed[0].token));
    assert!(!shared.idle(&member), "not drained yet");
    member.quiesce.drained.set(true);
    assert!(!shared.idle(&member), "the op is still in flight");
    for sqe in &pushed {
        script
            .borrow_mut()
            .answers
            .push(Cqe::err(sqe.token, IoError::Canceled));
    }
    host(&mut shared).reap(&mut Vec::new());
    assert!(shared.idle(&member));
    shared.end(&member);
    assert!(!member.quiesce.active.get() && !member.quiesce.drained.get());
}

#[test]
fn quiesce_with_no_reactor_is_idle_at_once() {
    let mut shared = crate::io::IoShared::default();
    let member = shared.member("gps").unwrap();
    shared.begin(&member);
    assert!(shared.idle(&member));
}

#[test]
fn unanswered_cancel_keeps_the_member_busy() {
    let (mut shared, member, _script) = quiesce_rig();
    let reactor = &mut shared.host.as_mut().unwrap().reactor;
    reactor.push(
        SourceId(0),
        Sqe::new(Token::new(1), RawHandle::new(0, 0), Op::CLOSE),
    );
    reactor.submit();
    shared.begin(&member);
    member.quiesce.drained.set(true);
    for _ in 0..10 {
        shared.host.as_mut().unwrap().reactor.reap(&mut Vec::new());
        assert!(!shared.idle(&member));
    }
}
