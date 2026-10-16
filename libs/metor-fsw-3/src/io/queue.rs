//! `Io`, a system's queue pair; `Pending<O>`, an op in flight; and `Slot<O>`,
//! which keeps a buffer at home in state. The op types live in `ops`.

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use super::arena::{Arena, Buf};
use super::op_slot::{Event, OpSlot, Write};
use super::ops::{BufOp, IoOp, RawOp};
use super::{Cqe, IoError, IoStatus, MAX_OPS, Op, RawHandle, Span, Sqe, Token};
use crate::port::{Input, Output};

struct Queue {
    sq: Output<Sqe>,
    cq: RefCell<Input<Cqe>>,
    ops: [Cell<OpSlot>; MAX_OPS],
    generation: Cell<u16>,
    arena: Rc<Arena>,
    next_grant: Cell<u16>,
}

/// A system's queue pair and arena.
#[derive(Clone)]
pub struct Io(Rc<Queue>);

impl Io {
    pub(crate) fn new(sq: Output<Sqe>, cq: Input<Cqe>, arena: Rc<Arena>) -> Self {
        Self(Rc::new(Queue {
            sq,
            cq: RefCell::new(cq),
            ops: core::array::from_fn(|_| Cell::new(OpSlot::FREE)),
            generation: Cell::new(0),
            arena,
            next_grant: Cell::new(0),
        }))
    }

    pub(crate) fn arena(&self) -> &Rc<Arena> {
        &self.0.arena
    }

    /// The handle of the next `Cap` bound to this queue: grant `n` is index `n`.
    pub(crate) fn next_grant(&self) -> RawHandle {
        let n = self.0.next_grant.get();
        self.0.next_grant.set(n.wrapping_add(1));
        RawHandle::new(n, 0)
    }

    /// Submits `op` on `handle`, lending it `buf`.
    pub(crate) fn submit<O: IoOp>(
        &self,
        handle: RawHandle,
        op: Op,
        buf: Option<Buf>,
    ) -> Pending<O> {
        Pending {
            state: self.queue_op(handle, &op, buf.as_ref()),
            io: self.clone(),
            buf,
            _o: PhantomData,
        }
    }

    /// Submits any op on any handle. Most systems use a `Handle` instead.
    pub fn submit_raw(&self, handle: RawHandle, op: Op, buf: Option<Buf>) -> Pending<RawOp> {
        self.submit(handle, op, buf)
    }

    /// Writes `op` to the SQ under a free slot's token.
    ///
    /// Fails without queueing if `buf` does not suit `op`, or if no slot or
    /// no SQ room is free.
    fn queue_op(&self, handle: RawHandle, op: &Op, buf: Option<&Buf>) -> OpState {
        let span = match buf {
            Some(buf) if buf.live() => buf.span_for(op),
            Some(_) => return OpState::failed(IoError::BadBuf),
            None if op.fills() || op.drains() => return OpState::failed(IoError::BadBuf),
            None => Span::default(),
        };
        self.pump();
        let Some(index) = self.0.ops.iter().position(|s| s.get().is_free()) else {
            return OpState::failed(IoError::QueueFull);
        };
        let generation = self.0.generation.get().wrapping_add(1);
        self.0.generation.set(generation);
        let token = Token::new(index as u16, generation);
        if self.0.sq.write(&Sqe::new(token, handle, op, span)).is_err() {
            return OpState::failed(IoError::QueueFull);
        }
        let opens = op.opens();
        self.apply(index, Event::Submitted { generation, opens });
        OpState::Queued(token)
    }

    /// Writes again what the SQ refused, then records every completion on
    /// the CQ.
    fn pump(&self) {
        for index in 0..MAX_OPS {
            self.apply(index, Event::Pumped);
        }
        let Ok(mut cq) = self.0.cq.try_borrow_mut() else {
            return;
        };
        for cqe in cq.drain().flatten() {
            let (generation, status, value) = (cqe.token.generation(), cqe.status, cqe.value.get());
            let event = Event::Completed {
                generation,
                status,
                value,
            };
            self.apply(usize::from(cqe.token.slot()), event);
        }
    }

    /// Applies `event` to slot `index` and writes what the slot asks for. A
    /// write the SQ refuses is reported back to the slot.
    fn apply(&self, index: usize, event: Event) {
        let Some(cell) = self.0.ops.get(index) else {
            return;
        };
        let (mut slot, write) = cell.get().on(event);
        if let Some(write) = write {
            let token = Token::new(index as u16, slot.generation);
            if !self.write(token, write) {
                slot = slot.on(Event::Refused(write)).0;
            }
        }
        cell.set(slot);
    }

    /// Writes a slot's cancel or close. False if the SQ is full.
    fn write(&self, token: Token, write: Write) -> bool {
        let (handle, op) = match write {
            Write::Cancel => (RawHandle::new(0, 0), Op::Cancel),
            Write::Close(handle) => (handle, Op::Close),
        };
        self.0
            .sq
            .write(&Sqe::new(token, handle, &op, Span::default()))
            .is_ok()
    }

    /// Takes a finished op's result, freeing its slot.
    fn take(&self, token: Token) -> Option<(IoStatus, u32)> {
        self.pump();
        let index = usize::from(token.slot());
        let generation = token.generation();
        let result = self.0.ops.get(index)?.get().result(generation)?;
        self.apply(index, Event::Taken { generation });
        Some(result)
    }

    /// Drops interest in an op.
    fn orphan(&self, token: Token) {
        let generation = token.generation();
        self.apply(usize::from(token.slot()), Event::Dropped { generation });
    }

    /// Cancels an op in flight. A cancel the SQ refuses is written again on
    /// a later pump.
    fn cancel(&self, token: Token) {
        let generation = token.generation();
        self.apply(usize::from(token.slot()), Event::Canceled { generation });
    }

    /// Ops not yet taken, or orphaned and not yet landed.
    pub fn in_flight(&self) -> usize {
        self.pump();
        self.0.ops.iter().filter(|s| !s.get().is_free()).count()
    }
}

/// Where a `Pending` op is.
enum OpState {
    Queued(Token),
    /// Finished without reaching the queue, such as a `QueueFull`.
    Failed(IoStatus),
    /// Its result was already returned.
    Returned,
}

impl OpState {
    fn failed(error: IoError) -> Self {
        Self::Failed(IoStatus::err(error))
    }
}

/// An op in flight.
///
/// Poll it once per cycle, or await it in a sequence. Dropping it leaves the
/// op running and discards its result; [`Pending::cancel`] cancels it.
#[must_use]
pub struct Pending<O: IoOp> {
    io: Io,
    state: OpState,
    buf: Option<Buf>,
    _o: PhantomData<fn() -> O>,
}

impl<O: IoOp> Pending<O> {
    /// The result once the op has completed. `None` before that, and after
    /// the result was taken.
    pub fn poll(&mut self) -> Option<O::Output> {
        let (status, value) = match self.state {
            OpState::Queued(token) => self.io.take(token)?,
            OpState::Failed(status) => (status, 0),
            OpState::Returned => return None,
        };
        self.state = OpState::Returned;
        Some(O::finish(&self.io, status, value, self.buf.take()))
    }

    /// The result if the op has completed, or the op back.
    pub fn try_take(mut self) -> Result<O::Output, Self> {
        self.poll().ok_or(self)
    }

    /// Cancels the op. Its completion still arrives, usually `Canceled`. The
    /// cancel takes no slot of its own.
    pub fn cancel(&self) {
        if let OpState::Queued(token) = self.state {
            self.io.cancel(token);
        }
    }
}

/// Resolves once the op has completed. Registers no waker: a sequence's step
/// polls it every cycle.
impl<O: IoOp> Future for Pending<O> {
    type Output = O::Output;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<O::Output> {
        match Pending::poll(self.get_mut()) {
            Some(out) => Poll::Ready(out),
            None => Poll::Pending,
        }
    }
}

impl<O: IoOp> Drop for Pending<O> {
    fn drop(&mut self) {
        if let OpState::Queued(token) = self.state {
            self.io.orphan(token);
        }
    }
}

/// A buffer kept in state, either idle or lent to an op.
pub enum Slot<O: BufOp> {
    Idle(Buf),
    Busy(Pending<O>),
    /// Between `take_idle` and storing the op that took the buffer.
    Empty,
}

impl<O: BufOp> Slot<O> {
    /// Takes the idle buffer to start an op.
    pub fn take_idle(&mut self) -> Option<Buf> {
        match core::mem::replace(self, Self::Empty) {
            Self::Idle(buf) => Some(buf),
            other => {
                *self = other;
                None
            }
        }
    }

    /// The op's result once it lands. Puts the buffer back as idle.
    pub fn poll(&mut self) -> Option<Result<O::Ok, IoError>> {
        let Self::Busy(pending) = self else {
            return None;
        };
        let (result, buf) = pending.poll()?;
        *self = Self::Idle(buf);
        Some(result)
    }

    /// The idle buffer's bytes, or none.
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Idle(buf) => buf,
            _ => &[],
        }
    }
}
