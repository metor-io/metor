mod arena;
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

pub(crate) use crate::io::tests::allocations;

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
