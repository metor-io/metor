mod buf;
mod cap;
mod harness;
mod kind;
mod op;
#[cfg(all(test, feature = "host"))]
mod param;
#[cfg(all(test, feature = "host", unix))]
mod poll;
mod queue;
#[cfg(all(test, feature = "host", unix))]
mod reactor;
mod slot;
#[cfg(all(test, feature = "host", unix))]
pub(crate) mod system;
#[cfg(all(test, feature = "host", target_os = "linux"))]
mod uring;

#[cfg(test)]
pub(super) use counting::allocations;
#[cfg(test)]
pub(crate) use rig::Rig;

#[cfg(test)]
mod rig {
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
    use zerocopy::IntoBytes;

    use crate::io::queue::{Io, Table};
    use crate::io::{Buf, Cqe, MAX_OPS, Sqe};
    use crate::port::{Input, Output, ring_capacity};
    use crate::record::{Bytes, Record};

    fn ring(max_len: usize, depth: usize) -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(max_len, depth).expect("valid capacity"),
            max_readers: 1,
        })
    }

    /// An `Io` over in-memory rings, with the test playing the reactor.
    pub struct Rig {
        pub io: Io,
        sq: Input<Sqe>,
        cq: Output<Bytes>,
        _rings: [RingBuffer; 2],
    }

    impl Rig {
        pub fn new() -> Self {
            Self::with_sq_depth(MAX_OPS)
        }

        /// A rig whose SQ ring holds `depth` ops.
        pub fn with_sq_depth(depth: usize) -> Self {
            let sq_ring = ring(Sqe::MAX_LEN, depth);
            let cq_ring = ring(Cqe::MAX_LEN, MAX_OPS);
            let sq_out = Output::new(sq_ring.writer(NoWake).expect("writer"));
            let cq_in = Input::new(vec![cq_ring.view(NoWake).expect("view")]);
            let io = Io::new(sq_out, cq_in, Table::new());
            let sq = Input::new(vec![sq_ring.view(NoWake).expect("view")]);
            let cq = Output::bytes(cq_ring.writer(NoWake).expect("writer"), Cqe::MAX_LEN);
            Self {
                io,
                sq,
                cq,
                _rings: [sq_ring, cq_ring],
            }
        }

        /// Allocates without an `IoInit`, so the table stays unfrozen and
        /// tests can keep allocating. Tests hold no slice across an alloc.
        pub fn buf(&self, bytes: &[u8]) -> Buf {
            let table = self.io.table();
            Buf::new(table, table.alloc_from(bytes))
        }

        pub fn empty_buf(&self, cap: usize) -> Buf {
            let table = self.io.table();
            Buf::new(table, table.alloc(cap))
        }

        /// Every op submitted since the last call.
        pub fn sent(&mut self) -> Vec<Sqe> {
            self.sq.drain().map(|s| s.expect("an sqe")).collect()
        }

        /// The token of the one op submitted since the last call. Does not
        /// allocate.
        pub fn sent_token(&mut self) -> crate::io::Token {
            let mut ops = self.sq.drain().map(|s| s.expect("an sqe").token);
            let token = ops.next().expect("one op");
            assert!(ops.next().is_none(), "one op");
            token
        }

        pub fn reply(&self, cqe: Cqe) {
            self.cq.write_bytes(cqe.as_bytes()).expect("cq room");
        }

        pub fn garbage(&self) {
            self.cq.write_bytes(&[0xFF; 40]).expect("cq room");
        }

        /// Fills slot `slot` of the table as the kernel would.
        pub fn kernel_fill(&self, slot: u16, bytes: &[u8]) {
            // SAFETY: the op holding this slot is in flight, so no `Buf`
            // reaches it, and the test plays the kernel.
            let whole = unsafe { self.io.table().bufs.whole_mut(slot) };
            whole[..bytes.len()].copy_from_slice(bytes);
        }
    }
}

#[cfg(test)]
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

    pub fn allocations(f: impl FnOnce()) -> usize {
        COUNT.with(|c| c.set(Some(0)));
        f();
        COUNT.with(|c| c.take()).unwrap_or(0)
    }
}
