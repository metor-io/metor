use crate::io::*;

#[cfg(test)]
mod unit {
    use super::super::{Rig, allocations};
    use super::*;

    fn read(rig: &Rig, buf: Buf) -> Pending<Read> {
        let op = Op::read(buf.buf_ref(), None);
        Pending::new(&rig.io, RawHandle::new(0, 0), op, Some(buf))
    }

    fn write(rig: &Rig, buf: Buf) -> Pending<Write> {
        let op = Op::write(buf.buf_ref(), None);
        Pending::new(&rig.io, RawHandle::new(0, 0), op, Some(buf))
    }

    #[test]
    fn read_completes_with_the_bytes() {
        let mut rig = Rig::new();
        let buf = rig.empty_buf(8);
        let slot = buf.buf_ref();
        let mut pending = read(&rig, buf);
        let sent = rig.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].op.buf(), Some(slot));
        assert!(pending.poll().is_none());
        rig.kernel_fill(slot.slot(), b"abc");
        rig.reply(Cqe::ok(sent[0].token, Out::len(3)));
        let (result, buf) = pending.poll().unwrap();
        assert_eq!(result, Ok(3));
        assert_eq!(&buf[..], b"abc");
        assert!(pending.poll().is_none());
    }

    #[test]
    fn write_hands_the_buffer_back_unchanged() {
        let mut rig = Rig::new();
        let mut pending = write(&rig, rig.buf(b"hello"));
        let token = rig.sent()[0].token;
        rig.reply(Cqe::ok(token, Out::len(5)));
        let (result, buf) = pending.poll().unwrap();
        assert_eq!(result, Ok(5));
        assert_eq!(&buf[..], b"hello");
    }

    #[test]
    fn slot_goes_idle_busy_idle() {
        let mut rig = Rig::new();
        let mut slot: Slot<Write> = Slot::Idle(rig.buf(b"x"));
        let buf = slot.take_idle().unwrap();
        assert!(slot.take_idle().is_none());
        slot = Slot::Busy(write(&rig, buf));
        assert!(slot.poll().is_none());
        assert!(slot.bytes().is_empty());
        let token = rig.sent()[0].token;
        rig.reply(Cqe::ok(token, Out::len(1)));
        assert_eq!(slot.poll(), Some(Ok(1)));
        assert_eq!(slot.bytes(), b"x");
    }

    #[test]
    fn one_op_past_max_is_queue_full() {
        let mut rig = Rig::new();
        let held: Vec<_> = (0..MAX_OPS).map(|_| write(&rig, rig.buf(b"a"))).collect();
        let mut extra = write(&rig, rig.buf(b"late"));
        let (result, buf) = extra.poll().unwrap();
        assert_eq!(result, Err(IoError::QueueFull));
        assert_eq!(&buf[..], b"late");
        assert_eq!(rig.sent().len(), MAX_OPS);
        drop(held);
    }

    #[test]
    fn dropped_op_frees_its_slot_when_it_lands() {
        let mut rig = Rig::new();
        drop(write(&rig, rig.buf(b"a")));
        assert_eq!(rig.io.in_flight(), 1);
        let token = rig.sent()[0].token;
        rig.reply(Cqe::ok(token, Out::len(1)));
        assert_eq!(rig.io.in_flight(), 0);
    }

    #[test]
    fn stale_and_duplicate_completions_are_skipped() {
        let mut rig = Rig::new();
        let mut first = write(&rig, rig.buf(b"a"));
        let token = rig.sent()[0].token;
        rig.reply(Cqe::ok(token, Out::len(1)));
        assert!(first.poll().is_some());
        let mut second = write(&rig, rig.buf(b"b"));
        let next = rig.sent()[0].token;
        assert_ne!(next, token);
        rig.reply(Cqe::ok(token, Out::len(9)));
        assert!(second.poll().is_none());
        rig.reply(Cqe::ok(next, Out::len(1)));
        rig.reply(Cqe::ok(next, Out::len(7)));
        assert_eq!(second.poll().map(|(r, _)| r), Some(Ok(1)));
        assert_eq!(rig.io.in_flight(), 0);
    }

    #[test]
    fn cancel_names_the_token() {
        let mut rig = Rig::new();
        let pending = read(&rig, rig.empty_buf(4));
        pending.cancel();
        let sent = rig.sent();
        assert_eq!(sent[1].op, Op::cancel(sent[0].token));
        assert_eq!(sent[1].handle, sent[0].handle);
    }

    #[test]
    fn completion_never_lands_during_submit() {
        let mut rig = Rig::new();
        let guess = Token::new(1 << 16);
        rig.reply(Cqe::ok(guess, Out::len(1)));
        let mut pending = write(&rig, rig.buf(b"a"));
        assert_eq!(rig.sent()[0].token, guess);
        assert!(pending.poll().is_none());
    }

    #[test]
    fn full_sq_ring_is_queue_full() {
        let mut rig = Rig::with_sq_depth(2);
        let held: Vec<_> = (0..2).map(|_| write(&rig, rig.buf(b"a"))).collect();
        let mut extra = write(&rig, rig.buf(b"b"));
        assert_eq!(extra.poll().map(|(r, _)| r), Some(Err(IoError::QueueFull)));
        assert_eq!(rig.io.in_flight(), 2);
        assert_eq!(rig.sent().len(), 2);
        drop(held);
    }

    #[test]
    fn garbage_completion_is_skipped() {
        let mut rig = Rig::new();
        let mut pending = write(&rig, rig.buf(b"a"));
        rig.garbage();
        let token = rig.sent()[0].token;
        rig.reply(Cqe::ok(token, Out::len(1)));
        assert_eq!(pending.poll().map(|(r, _)| r), Some(Ok(1)));
    }

    #[test]
    fn mismatched_out_is_device() {
        let mut rig = Rig::new();
        let mut pending = read(&rig, rig.empty_buf(4));
        let out = Out::handle(RawHandle::new(3, 0));
        let token = rig.sent()[0].token;
        rig.reply(Cqe::ok(token, out));
        assert_eq!(pending.poll().map(|(r, _)| r), Some(Err(IoError::Device)));
    }

    #[test]
    fn steady_rounds_do_not_allocate() {
        let mut rig = Rig::new();
        let mut slot: Slot<Write> = Slot::Idle(rig.buf(b"x"));
        let mut round = |rig: &mut Rig| {
            let buf = slot.take_idle().unwrap();
            slot = Slot::Busy(write(rig, buf));
            let token = rig.sent_token();
            rig.reply(Cqe::ok(token, Out::len(1)));
            assert_eq!(slot.poll(), Some(Ok(1)));
        };
        round(&mut rig);
        assert_eq!(allocations(|| (0..1000).for_each(|_| round(&mut rig))), 0);
    }

    /// Also run under Miri.
    #[test]
    fn deref_while_another_buf_is_written() {
        let rig = Rig::new();
        let a = rig.buf(b"left");
        let mut b = rig.buf(b"right");
        let view = &a[..];
        b.fill(b"other");
        b[0] = b'O';
        assert_eq!(view, b"left");
        assert_eq!(&b[..], b"Other");
    }

    /// Also run under Miri.
    #[test]
    fn io_drops_with_orphans_and_live_bufs() {
        let rig = Rig::new();
        let live = rig.buf(b"keep");
        drop(read(&rig, rig.empty_buf(4)));
        let pending = write(&rig, rig.buf(b"w"));
        drop(rig);
        assert_eq!(&live[..], b"keep");
        drop(pending);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;
    use crate::io::queue::Slots;

    /// No sequence of claims, completions, orphans, and takes holds more than
    /// `MAX_OPS` slots or hands one token to two live ops.
    #[kani::proof]
    #[kani::unwind(6)]
    fn token_slots_bounded() {
        let mut slots: Slots<()> = Slots::new();
        let mut live: [Option<Token>; MAX_OPS + 1] = [None; MAX_OPS + 1];
        for _ in 0..4 {
            let pick: usize = kani::any();
            kani::assume(pick < live.len());
            match kani::any::<u8>() % 4 {
                0 => {
                    if let Ok(token) = slots.claim(()) {
                        assert!(live.iter().flatten().all(|t| *t != token));
                        if let Some(free) = live.iter_mut().find(|t| t.is_none()) {
                            *free = Some(token);
                        }
                    }
                }
                1 => {
                    if let Some(token) = live[pick] {
                        slots.land(&Cqe::ok(token, Out::UNIT));
                    }
                }
                2 => {
                    if let Some(token) = live[pick].take() {
                        slots.orphan(token);
                    }
                }
                _ => {
                    if let Some(token) = live[pick] {
                        if slots.take(token).is_some() {
                            live[pick] = None;
                        }
                    }
                }
            }
            assert!(slots.live() <= MAX_OPS);
        }
    }
}
