use super::{Loopback, allocations};
use crate::io::cap::{Cap, Handle};
use crate::io::*;

fn uart(lb: &Loopback) -> Cap<Uart> {
    Cap::new(Handle::new(lb.io.next_grant(), lb.io.clone()))
}

#[test]
fn read_completes_with_its_buffer_and_length() {
    let mut lb = Loopback::new();
    let buf = lb.init().alloc(8);
    let uart = uart(&lb);
    let mut rx = Slot::Busy(uart.read(buf));
    let [sqe] = lb.submitted()[..] else { panic!() };
    assert_eq!(sqe.op(), Ok(Op::Read { offset: None }));
    assert_eq!(sqe.buf, Span::new(0, 8), "a read names its whole capacity");
    assert_eq!(sqe.handle, RawHandle::new(0, 0));
    assert!(rx.poll().is_none());
    lb.complete(Cqe::ok(sqe.token, 3));
    assert_eq!(rx.poll(), Some(Ok(3)));
    assert_eq!(rx.bytes().len(), 3);
    assert_eq!(lb.io.in_flight(), 0);
}

#[test]
fn write_names_its_filled_bytes() {
    let mut lb = Loopback::new();
    let mut buf = lb.init().alloc(8);
    buf.fill(b"hi");
    let _w = uart(&lb).write(buf);
    let [sqe] = lb.submitted()[..] else { panic!() };
    assert_eq!(sqe.buf, Span::new(0, 2));
}

#[test]
fn full_table_completes_queue_full_with_the_buffer_back() {
    let mut lb = Loopback::new();
    let init = lb.init();
    let bufs: Vec<_> = (0..=MAX_OPS).map(|_| init.alloc(1)).collect();
    drop(init);
    let uart = uart(&lb);
    let mut ops: Vec<_> = bufs.into_iter().map(|b| uart.read(b)).collect();
    let (result, buf) = ops.pop().unwrap().poll().unwrap();
    assert_eq!(result, Err(IoError::QueueFull));
    assert_eq!(buf.capacity(), 1);
    assert_eq!(lb.submitted().len(), MAX_OPS);
}

#[test]
fn buffer_op_without_a_live_buffer_is_bad_buf() {
    let lb = Loopback::new();
    let old = lb.init().alloc(4);
    drop(lb.init());
    let (result, _) = uart(&lb).read(old).poll().unwrap();
    assert_eq!(result, Err(IoError::BadBuf), "a buffer from an older make");
}

#[test]
fn dropped_pending_frees_its_slot_when_it_lands() {
    let mut lb = Loopback::new();
    drop(lb.init());
    let uart = uart(&lb);
    drop(uart.configure(TtyConfig::new(9600)));
    assert_eq!(lb.io.in_flight(), 1);
    let [sqe] = lb.submitted()[..] else { panic!() };
    lb.complete(Cqe::ok(sqe.token, 0));
    assert_eq!(lb.io.in_flight(), 0);
}

#[test]
fn orphaned_accept_closes_its_handle_under_its_own_token() {
    let mut lb = Loopback::new();
    drop(lb.init());
    let listen: Handle<Listen> = Handle::new(lb.io.next_grant(), lb.io.clone());
    drop(listen.accept());
    let [accept] = lb.submitted()[..] else {
        panic!()
    };
    let stream = RawHandle::new(5, 1);
    lb.complete(Cqe::ok(accept.token, stream.get()));
    assert_eq!(lb.io.in_flight(), 1, "the close holds the slot");
    let [close] = lb.submitted()[..] else {
        panic!()
    };
    assert_eq!(
        (close.op(), close.token, close.handle),
        (Ok(Op::Close), accept.token, stream)
    );
    lb.complete(Cqe::ok(close.token, 0));
    assert_eq!(lb.io.in_flight(), 0);
}

#[test]
fn finished_accept_dropped_untaken_closes_too() {
    let mut lb = Loopback::new();
    drop(lb.init());
    let listen: Handle<Listen> = Handle::new(lb.io.next_grant(), lb.io.clone());
    let accept = listen.accept();
    let [sqe] = lb.submitted()[..] else { panic!() };
    lb.complete(Cqe::ok(sqe.token, RawHandle::new(5, 1).get()));
    assert_eq!(lb.io.in_flight(), 1);
    drop(accept);
    let [close] = lb.submitted()[..] else {
        panic!()
    };
    assert_eq!(close.op(), Ok(Op::Close));
}

#[test]
fn accept_resolves_to_a_handle_on_the_same_queue() {
    let mut lb = Loopback::new();
    drop(lb.init());
    let listen: Handle<Listen> = Handle::new(lb.io.next_grant(), lb.io.clone());
    let mut accept = listen.accept();
    let [sqe] = lb.submitted()[..] else { panic!() };
    lb.complete(Cqe::ok(sqe.token, RawHandle::new(5, 1).get()));
    let stream = accept.poll().unwrap().unwrap();
    assert_eq!(stream.raw(), RawHandle::new(5, 1));
    drop(stream.close());
    let [close] = lb.submitted()[..] else {
        panic!()
    };
    assert_eq!(close.handle, RawHandle::new(5, 1));
}

#[test]
fn cancel_names_its_target_and_takes_no_slot() {
    let mut lb = Loopback::new();
    drop(lb.init());
    let uart = uart(&lb);
    let flush = uart.flush(TtyQueue::Input);
    flush.cancel();
    let [op, cancel] = lb.submitted()[..] else {
        panic!()
    };
    assert_eq!((cancel.op(), cancel.token), (Ok(Op::Cancel), op.token));
    assert_eq!(lb.io.in_flight(), 1);
}

#[test]
fn stale_generation_is_ignored() {
    let mut lb = Loopback::new();
    drop(lb.init());
    let uart = uart(&lb);
    let mut first = uart.flush(TtyQueue::Input);
    let [sqe] = lb.submitted()[..] else { panic!() };
    let stale = Token::new(sqe.token.slot(), sqe.token.generation().wrapping_sub(1));
    lb.complete(Cqe::ok(stale, 0));
    assert!(first.poll().is_none());
    lb.complete(Cqe::err(sqe.token, IoError::Os(Errno::EIO)));
    assert_eq!(first.poll(), Some(Err(IoError::Os(Errno::EIO))));
    assert!(first.poll().is_none(), "taken once");
}

#[test]
fn steady_state_allocates_nothing() {
    let mut lb = Loopback::new();
    let buf = lb.init().alloc(4);
    let uart = uart(&lb);
    let mut rx: Slot<ReadOp> = Slot::Idle(buf);
    let mut sqes = Vec::with_capacity(4);
    let n = allocations(|| {
        for _ in 0..8 {
            let buf = rx.take_idle().unwrap();
            rx = Slot::Busy(uart.read(buf));
            sqes.extend(lb.sq.drain().map(Result::unwrap));
            let sqe = sqes.pop().unwrap();
            lb.complete(Cqe::ok(sqe.token, 1));
            assert_eq!(rx.poll(), Some(Ok(1)));
        }
    });
    assert_eq!(n, 0);
}

/// Submits flushes until the SQ refuses one.
fn fill(uart: &Cap<Uart>) -> Vec<Pending<TtyOp>> {
    let mut held = Vec::new();
    loop {
        let mut flush = uart.flush(TtyQueue::Both);
        if flush.poll() == Some(Err(IoError::QueueFull)) {
            return held;
        }
        held.push(flush);
    }
}

#[test]
fn cancel_the_full_sq_refuses_goes_out_later() {
    let mut lb = Loopback::with_sq_depth(2);
    drop(lb.init());
    let uart = uart(&lb);
    let first = uart.flush(TtyQueue::Input);
    let _held = fill(&uart);
    first.cancel();
    let sent = lb.submitted();
    assert!(
        sent.iter().all(|s| s.op() != Ok(Op::Cancel)),
        "the SQ was full"
    );
    lb.io.in_flight();
    let [cancel] = lb.submitted()[..] else {
        panic!()
    };
    assert_eq!((cancel.op(), cancel.token), (Ok(Op::Cancel), sent[0].token));
    lb.io.in_flight();
    assert!(lb.submitted().is_empty(), "sent once");
}

#[test]
fn orphan_close_the_full_sq_refuses_goes_out_later() {
    let mut lb = Loopback::with_sq_depth(2);
    drop(lb.init());
    let listen: Handle<Listen> = Handle::new(lb.io.next_grant(), lb.io.clone());
    drop(listen.accept());
    let accept = lb.submitted()[0];
    let uart = uart(&lb);
    let held = fill(&uart);
    lb.complete(Cqe::ok(accept.token, RawHandle::new(5, 1).get()));
    lb.io.in_flight();
    assert_eq!(lb.submitted().len(), held.len(), "the close waits for room");
    lb.io.in_flight();
    let [close] = lb.submitted()[..] else {
        panic!()
    };
    assert_eq!((close.op(), close.token), (Ok(Op::Close), accept.token));
    lb.complete(Cqe::ok(close.token, 0));
    assert_eq!(lb.io.in_flight(), held.len());
}
