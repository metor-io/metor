//! `Io`, a system's queue pair; `Pending<O>`, an op in flight; and `Slot<O>`,
//! which keeps a buffer at home in state. The op types live in `ops`.

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use super::arena::{Arena, Buf};
use super::ops::{BufOp, IoOp, RawOp};
use super::{Cqe, IoError, IoStatus, MAX_OPS, Op, RawHandle, Span, Sqe, Token};
use crate::port::{Input, Output};

#[derive(Clone, Copy)]
enum SlotState {
    Free,
    Waiting {
        opens: bool,
    },
    /// Its `Pending` was dropped. The completion frees it, closing any handle
    /// the op opened first.
    Orphaned {
        opens: bool,
    },
    /// Closing the handle an orphaned op opened, under the op's own token.
    /// The close's completion frees it. A close the SQ refused is `sent`
    /// on a later pump.
    Closing {
        handle: RawHandle,
        sent: bool,
    },
    Done {
        status: IoStatus,
        value: u32,
        opens: bool,
    },
}

#[derive(Clone, Copy)]
struct OpSlot {
    generation: u16,
    state: SlotState,
    /// A cancel that did not fit in the SQ. The next pump writes it.
    cancel: bool,
}

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
        let free = OpSlot {
            generation: 0,
            state: SlotState::Free,
            cancel: false,
        };
        Self(Rc::new(Queue {
            sq,
            cq: RefCell::new(cq),
            ops: core::array::from_fn(|_| Cell::new(free)),
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
        let Some(slot) = self
            .0
            .ops
            .iter()
            .position(|s| matches!(s.get().state, SlotState::Free))
        else {
            return OpState::failed(IoError::QueueFull);
        };
        let generation = self.0.generation.get().wrapping_add(1);
        self.0.generation.set(generation);
        let token = Token::new(slot as u16, generation);
        if self.0.sq.write(&Sqe::new(token, handle, op, span)).is_err() {
            return OpState::failed(IoError::QueueFull);
        }
        let opens = op.opens();
        self.0.ops[slot].set(OpSlot {
            generation,
            state: SlotState::Waiting { opens },
            cancel: false,
        });
        OpState::Queued(token)
    }

    /// Writes again what the SQ refused, then records every completion on
    /// the CQ.
    fn pump(&self) {
        for (at, cell) in self.0.ops.iter().enumerate() {
            let slot = cell.get();
            let token = Token::new(at as u16, slot.generation);
            cell.set(self.resend(token, slot));
        }
        let Ok(mut cq) = self.0.cq.try_borrow_mut() else {
            return;
        };
        for cqe in cq.drain().flatten() {
            self.record_completion(cqe);
        }
    }

    /// `slot` after writing its refused cancel or close, if the SQ takes it.
    fn resend(&self, token: Token, mut slot: OpSlot) -> OpSlot {
        let in_flight = matches!(
            slot.state,
            SlotState::Waiting { .. } | SlotState::Orphaned { .. }
        );
        slot.cancel =
            slot.cancel && in_flight && !self.write(token, RawHandle::new(0, 0), &Op::Cancel);
        if let SlotState::Closing {
            handle,
            sent: false,
        } = slot.state
        {
            let sent = self.write(token, handle, &Op::Close);
            slot.state = SlotState::Closing { handle, sent };
        }
        slot
    }

    /// Writes an op that takes no buffer. False if the SQ is full.
    fn write(&self, token: Token, handle: RawHandle, op: &Op) -> bool {
        self.0
            .sq
            .write(&Sqe::new(token, handle, op, Span::default()))
            .is_ok()
    }

    /// Records `cqe` on its op's slot. A completion for a dropped op frees the
    /// slot, closing any handle the op opened first.
    fn record_completion(&self, cqe: Cqe) {
        let Some(cell) = self.0.ops.get(usize::from(cqe.token.slot())) else {
            return;
        };
        let mut slot = cell.get();
        if slot.generation != cqe.token.generation() {
            return;
        }
        slot.state = match slot.state {
            SlotState::Waiting { opens } => SlotState::Done {
                status: cqe.status,
                value: cqe.value.get(),
                opens,
            },
            SlotState::Orphaned { opens: true } => self.close_orphan(cqe.token, cqe.result()),
            done @ SlotState::Done { .. } => done,
            SlotState::Orphaned { .. } | SlotState::Closing { .. } | SlotState::Free => {
                SlotState::Free
            }
        };
        cell.set(slot);
    }

    /// Closes the handle an orphaned op opened, under the op's own token.
    fn close_orphan(&self, token: Token, opened: Result<u32, IoError>) -> SlotState {
        let Ok(raw) = opened else {
            return SlotState::Free;
        };
        let handle = RawHandle::from_raw(raw);
        let sent = self.write(token, handle, &Op::Close);
        SlotState::Closing { handle, sent }
    }

    /// Takes a finished op's result, freeing its slot.
    fn take(&self, token: Token) -> Option<(IoStatus, u32)> {
        self.pump();
        let cell = self.0.ops.get(usize::from(token.slot()))?;
        let slot = cell.get();
        let SlotState::Done { status, value, .. } = slot.state else {
            return None;
        };
        if slot.generation != token.generation() {
            return None;
        }
        cell.set(OpSlot {
            state: SlotState::Free,
            ..slot
        });
        Some((status, value))
    }

    /// Drops interest in an op.
    fn orphan(&self, token: Token) {
        let Some(cell) = self.0.ops.get(usize::from(token.slot())) else {
            return;
        };
        let mut slot = cell.get();
        if slot.generation != token.generation() {
            return;
        }
        slot.state = match slot.state {
            SlotState::Waiting { opens } => SlotState::Orphaned { opens },
            SlotState::Done {
                status,
                value,
                opens: true,
            } => self.close_orphan(token, status.result().map(|()| value)),
            _ => SlotState::Free,
        };
        cell.set(slot);
    }

    /// Cancels an op in flight. A cancel the SQ refuses is written again on
    /// a later pump.
    fn cancel(&self, token: Token) {
        let Some(cell) = self.0.ops.get(usize::from(token.slot())) else {
            return;
        };
        let mut slot = cell.get();
        if slot.generation == token.generation() {
            slot.cancel = true;
            cell.set(self.resend(token, slot));
        }
    }

    /// Ops not yet taken, or orphaned and not yet landed.
    pub fn in_flight(&self) -> usize {
        self.pump();
        let used = |s: &&Cell<OpSlot>| !matches!(s.get().state, SlotState::Free);
        self.0.ops.iter().filter(used).count()
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
