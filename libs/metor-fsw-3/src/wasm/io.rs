//! Copies IO buffer bytes between a wasm guest's arena and the host's mirror
//! of it.
//!
//! The mirror has the guest arena's length, and every op names a span of
//! both. The dispatch and the kernel use the mirror. A write's bytes go to
//! the mirror before its op reaches the real ring; a read's bytes come back
//! before the guest polls it.

use core::ptr::NonNull;
use std::rc::Rc;

use crate::io::{Arena, Cqe, MAX_OPS, Span, Sqe, Token};
use crate::mirror::Corrupt;
use crate::port::Input;

/// A region's base and length.
type Region = (NonNull<u8>, usize);

pub(super) struct IoCopy {
    /// The guest's arena, seen from the host.
    guest: Region,
    mirror: Rc<Arena>,
    /// A second reader on the guest's copy of `io_sq`.
    sq: Input<Sqe>,
    /// A second reader on the guest's copy of `io_cq`.
    cq: Input<Cqe>,
    /// Each read in flight, by its token's slot.
    reads: [Option<(Token, Span)>; MAX_OPS],
}

impl IoCopy {
    /// # Safety
    /// `guest` stays live and unmoved while the copy lives.
    pub(super) unsafe fn new(
        guest: Region,
        mirror: Rc<Arena>,
        sq: Input<Sqe>,
        cq: Input<Cqe>,
    ) -> Self {
        Self {
            guest,
            mirror,
            sq,
            cq,
            reads: [None; MAX_OPS],
        }
    }

    /// Copies each finished read's bytes from the mirror into the guest.
    pub(super) fn completions(&mut self) -> Result<(), Corrupt> {
        for cqe in self.cq.drain() {
            let cqe = cqe.map_err(|_| Corrupt)?;
            let read = self.reads.get_mut(usize::from(cqe.token.slot()));
            let Some((_, span)) = read.and_then(|r| r.take_if(|(t, _)| *t == cqe.token)) else {
                continue;
            };
            let n = cqe.result().unwrap_or(0).min(span.len());
            if let Some(mirror) = self.mirror.region() {
                copy(mirror, self.guest, Span::new(span.off(), n));
            }
        }
        Ok(())
    }

    /// Notes each new read, and copies each new write's bytes from the guest
    /// into the mirror.
    pub(super) fn submissions(&mut self) -> Result<(), Corrupt> {
        for sqe in self.sq.drain() {
            let sqe = sqe.map_err(|_| Corrupt)?;
            let Ok(op) = sqe.op() else { continue };
            if op.fills()
                && let Some(read) = self.reads.get_mut(usize::from(sqe.token.slot()))
            {
                *read = Some((sqe.token, sqe.buf));
            }
            if let (true, Some(mirror)) = (op.drains(), self.mirror.region()) {
                copy(self.guest, mirror, sqe.buf);
            }
        }
        Ok(())
    }
}

/// Copies `span` from `from` to `to`. A span outside either is left for the
/// dispatch to refuse.
fn copy(from: Region, to: Region, span: Span) {
    let Some(end) = span.end() else { return };
    if end as usize > from.1 || end as usize > to.1 {
        return;
    }
    let off = span.off() as usize;
    // SAFETY: the span lies in both regions, which are distinct, and only
    // this step touches them between guest calls.
    unsafe {
        let (src, dst) = (from.0.add(off), to.0.add(off));
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_ptr(), span.len() as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{Cqe, IoError, Op, RawHandle};
    use crate::port::Output;
    use crate::tests::utils::pair;

    /// A guest arena holding `cmd` then 8 free bytes, the host's mirror of
    /// it, and the guest's IO rings, which the test writes as both sides.
    struct Rig {
        copy: IoCopy,
        guest: Box<[u8]>,
        mirror: Rc<Arena>,
        sq: Output<Sqe>,
        cq: Output<Cqe>,
        _rings: [metor_fsw_3_ring::RingBuffer; 2],
    }

    fn rig() -> Rig {
        let mut guest: Box<[u8]> = vec![0; 11].into();
        guest[..3].copy_from_slice(b"cmd");
        let mirror = Rc::new(Arena::owning(vec![0; 11].into()));
        let (sq_ring, sq, sq_in) = pair::<Sqe>(16);
        let (cq_ring, cq, cq_in) = pair::<Cqe>(16);
        let region = (NonNull::from(&mut guest[..]).cast(), guest.len());
        // SAFETY: the boxed guest bytes never move and outlive the copy.
        let copy = unsafe { IoCopy::new(region, mirror.clone(), sq_in, cq_in) };
        Rig {
            copy,
            guest,
            mirror,
            sq,
            cq,
            _rings: [sq_ring, cq_ring],
        }
    }

    impl Rig {
        fn submit(&self, slot: u16, op: Op, span: Span) -> Token {
            let token = Token::new(slot, 1);
            self.sq
                .write(&Sqe::new(token, RawHandle::new(0, 0), &op, span))
                .unwrap();
            token
        }

        fn mirror(&self, span: Span) -> Vec<u8> {
            // SAFETY: nothing else touches the mirror during the test.
            unsafe { self.mirror.span(span).unwrap().as_ref() }.to_vec()
        }
    }

    #[test]
    fn write_bytes_reach_the_mirror() {
        let mut rig = rig();
        rig.submit(0, Op::Write { offset: None }, Span::new(0, 3));
        rig.copy.submissions().unwrap();
        assert_eq!(rig.mirror(Span::new(0, 3)), b"cmd");
    }

    #[test]
    fn read_bytes_reach_the_guest_on_completion() {
        let mut rig = rig();
        let token = rig.submit(3, Op::Recv, Span::new(3, 8));
        rig.copy.submissions().unwrap();
        // SAFETY: nothing else touches the mirror during the test.
        unsafe { rig.mirror.span(Span::new(3, 2)).unwrap().as_mut() }.copy_from_slice(b"ok");
        rig.cq.write(&Cqe::ok(token, 2)).unwrap();
        rig.copy.completions().unwrap();
        assert_eq!(&rig.guest[3..6], b"ok\0");
    }

    #[test]
    fn empty_or_failed_read_copies_nothing() {
        let mut rig = rig();
        // SAFETY: nothing else touches the mirror during the test.
        unsafe { rig.mirror.span(Span::new(3, 2)).unwrap().as_mut() }.copy_from_slice(b"ok");
        for (slot, ok) in [(4, true), (5, false)] {
            let token = rig.submit(slot, Op::Recv, Span::new(3, 8));
            rig.copy.submissions().unwrap();
            let cqe = match ok {
                true => Cqe::ok(token, 0),
                false => Cqe::err(token, IoError::Canceled),
            };
            rig.cq.write(&cqe).unwrap();
            rig.copy.completions().unwrap();
        }
        assert_eq!(&rig.guest[3..5], [0, 0]);
    }

    #[test]
    fn span_outside_the_arena_is_left_for_the_dispatch() {
        let mut rig = rig();
        rig.submit(0, Op::Write { offset: None }, Span::new(9, 8));
        assert_eq!(rig.copy.submissions(), Ok(()));
    }

    #[test]
    fn copying_allocates_nothing() {
        let mut rig = rig();
        let n = crate::io::tests::allocations(|| {
            for slot in 0..8 {
                let token = rig.submit(slot, Op::Recv, Span::new(3, 8));
                rig.copy.submissions().unwrap();
                rig.cq.write(&Cqe::ok(token, 1)).unwrap();
                rig.copy.completions().unwrap();
            }
        });
        assert_eq!(n, 0);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// For any span, `copy` reads and writes only inside both regions.
    #[kani::proof]
    fn copy_stays_in_bounds() {
        let mut a = [0u8; 8];
        let mut b = [0u8; 6];
        let from = (NonNull::from(&mut a[..]).cast(), a.len());
        let to = (NonNull::from(&mut b[..]).cast(), b.len());
        copy(from, to, Span::new(kani::any(), kani::any()));
    }
}
