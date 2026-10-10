//! Copies IO buffer bytes between a wasm guest's arena and the host's mirror
//! of it.
//!
//! The guest and the mirror have the same directory: the mirror is a copy of
//! the arena the guest laid out. The reactor and the kernel use the mirror. A
//! write's bytes, and its length, go to the mirror before its op reaches the
//! real ring; a read's bytes come back before the guest polls it.

use std::rc::Rc;

use crate::io::{BufRef, BufTable, Corrupt, Cqe, MAX_OPS, Sqe, Token, fills};
use crate::port::Input;

pub(super) struct IoCopy {
    /// The guest's arena, seen from the host.
    guest: BufTable,
    mirror: Rc<BufTable>,
    /// A second reader on the guest's copy of `io_sq`.
    sq: Input<Sqe>,
    /// A second reader on the guest's copy of `io_cq`.
    cq: Input<Cqe>,
    /// Each read in flight, by its token's slot.
    reads: Box<[Option<(Token, BufRef)>]>,
}

impl IoCopy {
    pub(super) fn new(
        guest: BufTable,
        mirror: Rc<BufTable>,
        sq: Input<Sqe>,
        cq: Input<Cqe>,
    ) -> Self {
        Self {
            guest,
            mirror,
            sq,
            cq,
            reads: vec![None; MAX_OPS].into(),
        }
    }

    /// Copies each finished read's bytes from the mirror into the guest.
    pub(super) fn completions(&mut self) -> Result<(), Corrupt> {
        let Self {
            cq,
            reads,
            guest,
            mirror,
            ..
        } = self;
        for cqe in cq.drain() {
            let cqe = cqe.map_err(|_| Corrupt)?;
            let Some(read) = slot(cqe.token).and_then(|at| reads.get_mut(at)) else {
                continue;
            };
            let Some((_, buf)) = read.take_if(|(token, _)| *token == cqe.token) else {
                continue;
            };
            let n = cqe.status.result().ok().and(cqe.out.as_len()).unwrap_or(0);
            copy(mirror, guest, buf.slot(), n)?;
        }
        Ok(())
    }

    /// Notes each new read, and copies each new write's length and bytes from
    /// the guest into the mirror.
    pub(super) fn submissions(&mut self) -> Result<(), Corrupt> {
        let Self {
            sq,
            reads,
            guest,
            mirror,
            ..
        } = self;
        for sqe in sq.drain() {
            let sqe = sqe.map_err(|_| Corrupt)?;
            let Some(buf) = sqe.op.buf() else { continue };
            if fills(&sqe.op) {
                if let Some(read) = slot(sqe.token).and_then(|at| reads.get_mut(at)) {
                    *read = Some((sqe.token, buf));
                }
                continue;
            }
            let Some((_, _, len)) = guest.entry(buf.slot()) else {
                continue;
            };
            mirror.set_len(buf.slot(), len).map_err(|_| Corrupt)?;
            copy(guest, mirror, buf.slot(), len)?;
        }
        Ok(())
    }
}

/// The slot an op's token names, if its system tracks it.
fn slot(token: Token) -> Option<usize> {
    let at = (token.get() & 0xFFFF) as usize;
    (at < MAX_OPS).then_some(at)
}

/// Copies `n` bytes of `slot` from `from` to `to`. Both tables must hold the
/// slot with the same capacity, of at least `n` bytes; a slot neither holds
/// is left for the reactor to refuse.
fn copy(from: &BufTable, to: &BufTable, slot: u16, n: u32) -> Result<(), Corrupt> {
    match (from.entry(slot), to.entry(slot)) {
        (None, None) => Ok(()),
        (Some((_, a, _)), Some((_, b, _))) if a == b && n <= a => {
            let (Some(src), Some(dst)) = (from.raw(slot), to.raw(slot)) else {
                return Err(Corrupt);
            };
            // SAFETY: both spans hold at least `n` bytes, lie in different
            // arenas, and only this step touches them between guest calls.
            unsafe {
                core::ptr::copy_nonoverlapping(src.ptr.as_ptr(), dst.ptr.as_ptr(), n as usize)
            };
            Ok(())
        }
        _ => Err(Corrupt),
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer};

    use super::*;
    use crate::io::region::Region;
    use crate::io::{BufRef, IoError, Op, Out, RawHandle};
    use crate::port::{Output, ring_capacity};
    use crate::record::Record;

    /// A guest's arena and the host's mirror of it, with the guest's IO
    /// rings, which the test writes as the guest and the host.
    struct Rig {
        copy: IoCopy,
        guest: Rc<BufTable>,
        sq: Output<Sqe>,
        cq: Output<Cqe>,
        _rings: [RingBuffer; 2],
    }

    fn ring<T: Record>() -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(T::MAX_LEN, 16).unwrap(),
            max_readers: 1,
        })
    }

    /// Two slots: a 3-byte write buffer holding `cmd`, and an 8-byte read
    /// buffer.
    fn rig() -> Rig {
        let made = BufTable::new(4);
        made.alloc_from(b"cmd").unwrap();
        made.alloc(8).unwrap();
        made.freeze();
        let bytes: Box<[u8]> = unsafe { made.region().unwrap().as_ref() }.into();
        let guest = Rc::new(BufTable::from_region(Region::mirror(bytes.clone())).unwrap());
        let mirror = Rc::new(BufTable::from_region(Region::mirror(bytes)).unwrap());
        mirror.set_len(0, 0).unwrap();
        let view = {
            let span = guest.region().unwrap();
            unsafe { Region::unowned(span.cast(), span.len()) }
        };
        let view = BufTable::from_region(view).unwrap();
        let (sq_ring, cq_ring) = (ring::<Sqe>(), ring::<Cqe>());
        let sq = Input::new(vec![sq_ring.view(NoWake).unwrap()]);
        let cq = Input::new(vec![cq_ring.view(NoWake).unwrap()]);
        Rig {
            copy: IoCopy::new(view, mirror, sq, cq),
            guest,
            sq: Output::new(sq_ring.writer(NoWake).unwrap()),
            cq: Output::new(cq_ring.writer(NoWake).unwrap()),
            _rings: [sq_ring, cq_ring],
        }
    }

    impl Rig {
        fn submit(&self, token: u32, op: Op) {
            self.sq
                .write(&Sqe::new(Token::new(token), RawHandle::new(0, 0), op))
                .unwrap();
        }

        fn complete(&self, cqe: Cqe) {
            self.cq.write(&cqe).unwrap();
        }

        fn mirror(&self, slot: u16) -> Vec<u8> {
            unsafe { self.copy.mirror.filled(slot) }.to_vec()
        }
    }

    #[test]
    fn write_bytes_and_length_reach_the_mirror() {
        let mut rig = rig();
        rig.submit(0, Op::write(BufRef::new(0), None));
        rig.copy.submissions().unwrap();
        assert_eq!(rig.mirror(0), b"cmd");
        rig.guest.set_len(0, 1).unwrap();
        rig.submit(1, Op::write(BufRef::new(0), None));
        rig.copy.submissions().unwrap();
        assert_eq!(rig.mirror(0), b"c", "a shorter write sends its new length");
    }

    #[test]
    fn read_bytes_reach_the_guest_on_completion() {
        let mut rig = rig();
        rig.submit(3, Op::read(BufRef::new(1), None));
        rig.copy.submissions().unwrap();
        let space = unsafe { rig.copy.mirror.whole_mut(1) };
        space[..2].copy_from_slice(b"ok");
        rig.complete(Cqe::ok(Token::new(3), Out::len(2)));
        rig.copy.completions().unwrap();
        assert_eq!(&unsafe { rig.guest.whole_mut(1) }[..2], b"ok");
    }

    #[test]
    fn empty_or_failed_read_copies_nothing() {
        let mut rig = rig();
        let space = unsafe { rig.copy.mirror.whole_mut(1) };
        space[..2].copy_from_slice(b"ok");
        for (token, cqe) in [
            (4, Cqe::ok(Token::new(4), Out::len(0))),
            (5, Cqe::err(Token::new(5), IoError::Canceled)),
        ] {
            rig.submit(token, Op::read(BufRef::new(1), None));
            rig.copy.submissions().unwrap();
            rig.complete(cqe);
            rig.copy.completions().unwrap();
        }
        assert_eq!(&unsafe { rig.guest.whole_mut(1) }[..2], [0, 0]);
    }

    #[test]
    fn ref_neither_holds_is_left_for_the_reactor() {
        let mut rig = rig();
        rig.submit(0, Op::write(BufRef::new(3), None));
        assert_eq!(rig.copy.submissions(), Ok(()));
    }

    #[test]
    fn guest_directory_unlike_the_mirror_is_corrupt() {
        let mut rig = rig();
        rig.guest.release(0);
        rig.guest.release(1);
        rig.guest.rewind().unwrap();
        rig.guest.alloc(2).unwrap();
        rig.submit(0, Op::write(BufRef::new(0), None));
        assert_eq!(rig.copy.submissions(), Err(Corrupt));
    }

    #[test]
    fn copying_allocates_nothing() {
        let mut rig = rig();
        let n = crate::io::tests::allocations(|| {
            for t in 0..8 {
                rig.submit(t, Op::read(BufRef::new(1), None));
                rig.copy.submissions().unwrap();
                rig.complete(Cqe::ok(Token::new(t), Out::len(1)));
                rig.copy.completions().unwrap();
            }
        });
        assert_eq!(n, 0);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;
    use crate::io::region::Region;

    /// A header, two entries, and 16 data bytes.
    const LEN: usize = 12 + 2 * 16 + 16;

    fn table() -> Option<BufTable> {
        let bytes: [u8; LEN] = kani::any();
        BufTable::from_region(Region::mirror(Box::new(bytes))).ok()
    }

    /// For any two directories, slot, and length, `copy` reads and writes
    /// inside both arenas; Kani checks every pointer it uses.
    #[kani::proof]
    #[kani::unwind(4)]
    fn copy_stays_in_bounds() {
        let (Some(from), Some(to)) = (table(), table()) else {
            return;
        };
        let _ = copy(&from, &to, kani::any::<u16>() % 3, kani::any());
    }
}
