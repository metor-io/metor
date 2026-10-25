mod arena;
#[cfg(all(feature = "host", unix))]
mod dispatch;
mod harness;
mod queue;
mod wire;

use std::rc::Rc;

use metor_fsw_3_ring::RingBuffer;

use super::arena::Arena;
use super::{Cqe, DEFAULT_OPS, Io, IoInit, Sqe};
use crate::coordinator::MakeError;
use crate::port::{Input, Output};
use crate::record::Record;
use crate::tests::utils::pair;

pub(crate) use counting::allocations;

/// An arena that owns `len` zeroed bytes.
pub(crate) fn arena(len: usize) -> Arena {
    let arena = Arena::new();
    arena.set_owned(vec![0; len].into());
    arena
}

/// A system's queue whose other end the test plays by hand.
pub(crate) struct Loopback {
    pub io: Io,
    pub sq: Input<Sqe>,
    pub cq: Output<Cqe>,
    _rings: [RingBuffer; 2],
}

impl Loopback {
    pub fn new() -> Self {
        Self::with_sq_depth(Sqe::DEPTH)
    }

    /// A loopback whose SQ holds `depth` records.
    pub fn with_sq_depth(depth: usize) -> Self {
        Self::with(depth, DEFAULT_OPS)
    }

    /// A loopback with `ops` op slots, its SQ sized to match.
    pub fn with_ops(ops: u16) -> Self {
        Self::with(Sqe::depth(ops), ops)
    }

    fn with(depth: usize, ops: u16) -> Self {
        let (sq_ring, sq_out, sq) = pair::<Sqe>(depth);
        let (cq_ring, cq, cq_in) = pair::<Cqe>(usize::from(ops));
        Self {
            io: Io::new(sq_out, cq_in, Rc::new(Arena::new()), ops),
            sq,
            cq,
            _rings: [sq_ring, cq_ring],
        }
    }

    pub fn make<S>(&self, ctor: impl FnOnce(&mut IoInit<'_>) -> Result<S, MakeError>) -> S {
        let mut init = IoInit::new(self.io.arena(), "test");
        // PANIC Safety: a test's buffers fit, or the test is wrong.
        let state = ctor(&mut init).expect("the test's buffers fit");
        init.finish();
        state
    }

    /// The records on the SQ, whose room the ring gives back on its next
    /// read.
    pub fn submitted(&mut self) -> Vec<Sqe> {
        let sqes = self.sq.drain().map(Result::unwrap).collect();
        assert_eq!(self.sq.drain().count(), 0);
        sqes
    }

    pub fn complete(&mut self, cqe: Cqe) {
        self.cq.write(&cqe).unwrap();
    }
}

mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    /// Counts allocations on threads that enable counting.
    struct Counting;

    thread_local! {
        static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
    }

    fn count() {
        let _ = COUNT.try_with(|c| c.set(c.get().map(|n| n + 1)));
    }

    // SAFETY: every method forwards to `System` with the caller's arguments.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count();
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            count();
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            count();
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    /// The allocations `f` makes on this thread.
    pub fn allocations(f: impl FnOnce()) -> usize {
        COUNT.with(|c| c.set(Some(0)));
        f();
        COUNT.with(|c| c.take()).unwrap_or(0)
    }
}
