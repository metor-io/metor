mod arena;
mod device;
#[cfg(all(feature = "host", unix))]
mod dispatch;
mod queue;
mod wire;

use std::rc::Rc;

use metor_fsw_3_ring::RingBuffer;

use super::arena::Arena;
use super::{Cqe, Io, IoInit, Sqe};
use crate::port::{Input, Output};
use crate::record::Record;
use crate::tests::utils::pair;

pub(crate) use counting::allocations;

/// A system's queue whose other end the test plays by hand.
pub(crate) struct Loopback {
    pub io: Io,
    pub sq: Input<Sqe>,
    pub cq: Output<Cqe>,
    _rings: [RingBuffer; 2],
}

impl Loopback {
    pub fn new() -> Self {
        let (sq_ring, sq_out, sq) = pair::<Sqe>(Sqe::DEPTH);
        let (cq_ring, cq, cq_in) = pair::<Cqe>(Cqe::DEPTH);
        Self {
            io: Io::new(sq_out, cq_in, Rc::new(Arena::new())),
            sq,
            cq,
            _rings: [sq_ring, cq_ring],
        }
    }

    pub fn init(&self) -> IoInit<'_> {
        IoInit::new(self.io.arena())
    }

    pub fn submitted(&mut self) -> Vec<Sqe> {
        self.sq.drain().map(Result::unwrap).collect()
    }

    pub fn complete(&self, cqe: Cqe) {
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
