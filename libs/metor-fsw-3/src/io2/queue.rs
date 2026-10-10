//! `Io`, a system's queue pair; `Pending<O>`, an op in flight; and `Slot<O>`,
//! which keeps a buffer at home in state.

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use super::arena::{Arena, Buf};
use super::cap::{File, Handle, Stream};
use super::{Cqe, IoError, IoStatus, MAX_OPS, Op, RawHandle, Span, Sqe, Token};
use crate::port::{Input, Output};

#[derive(Clone, Copy)]
enum State {
    Free,
    Waiting {
        opens: bool,
    },
    /// Its `Pending` was dropped. The completion frees it, closing any handle
    /// the op opened first.
    Orphan {
        opens: bool,
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
    state: State,
}

struct Queue {
    sq: Output<Sqe>,
    cq: RefCell<Input<Cqe>>,
    ops: [Cell<OpSlot>; MAX_OPS],
    generation: Cell<u16>,
    arena: Rc<Arena>,
    next_grant: Cell<u16>,
}

/// A system's queue pair and arena, shared by `Rc`.
///
/// Handles and pending ops each hold one.
#[derive(Clone)]
pub struct Io(Rc<Queue>);

impl Io {
    pub(crate) fn new(sq: Output<Sqe>, cq: Input<Cqe>, arena: Rc<Arena>) -> Self {
        let free = OpSlot {
            generation: 0,
            state: State::Free,
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
            state: self.start(handle, &op, buf.as_ref()),
            io: self.clone(),
            buf,
            _o: PhantomData,
        }
    }

    /// Submits any op on any handle. Most systems use a `Handle` instead.
    pub fn submit_raw(&self, handle: RawHandle, op: Op, buf: Option<Buf>) -> Pending<Raw> {
        self.submit(handle, op, buf)
    }

    fn start(&self, handle: RawHandle, op: &Op, buf: Option<&Buf>) -> Stage {
        let local = |e| Stage::Local(IoStatus::err(e));
        let span = match buf {
            Some(buf) if buf.live() => buf.span_for(op),
            Some(_) => return local(IoError::BadBuf),
            None if op.fills() || op.drains() => return local(IoError::BadBuf),
            None => Span::default(),
        };
        self.pump();
        let Some(slot) = self
            .0
            .ops
            .iter()
            .position(|s| matches!(s.get().state, State::Free))
        else {
            return local(IoError::QueueFull);
        };
        let generation = self.0.generation.get().wrapping_add(1);
        self.0.generation.set(generation);
        let token = Token::new(slot as u16, generation);
        if self.0.sq.write(&Sqe::new(token, handle, op, span)).is_err() {
            return local(IoError::QueueFull);
        }
        let opens = op.opens();
        self.0.ops[slot].set(OpSlot {
            generation,
            state: State::Waiting { opens },
        });
        Stage::Queued(token)
    }

    /// Records every completion on the CQ.
    fn pump(&self) {
        let Ok(mut cq) = self.0.cq.try_borrow_mut() else {
            return;
        };
        for cqe in cq.drain().flatten() {
            self.land(cqe);
        }
    }

    fn land(&self, cqe: Cqe) {
        let Some(cell) = self.0.ops.get(usize::from(cqe.token.slot())) else {
            return;
        };
        let mut slot = cell.get();
        if slot.generation != cqe.token.generation() {
            return;
        }
        slot.state = match slot.state {
            State::Waiting { opens } => State::Done {
                status: cqe.status,
                value: cqe.value.get(),
                opens,
            },
            State::Orphan { opens: true } => self.close_orphan(cqe.token, cqe.result()),
            done @ State::Done { .. } => done,
            State::Orphan { .. } | State::Free => State::Free,
        };
        cell.set(slot);
    }

    /// Closes the handle an orphaned op opened, under the op's own token.
    fn close_orphan(&self, token: Token, opened: Result<u32, IoError>) -> State {
        let Ok(raw) = opened else {
            return State::Free;
        };
        let close = Sqe::new(token, RawHandle::from_raw(raw), &Op::Close, Span::default());
        match self.0.sq.write(&close) {
            Ok(()) => State::Orphan { opens: false },
            Err(_) => State::Free,
        }
    }

    /// Takes a finished op's result, freeing its slot.
    fn take(&self, token: Token) -> Option<(IoStatus, u32)> {
        self.pump();
        let cell = self.0.ops.get(usize::from(token.slot()))?;
        let slot = cell.get();
        let State::Done { status, value, .. } = slot.state else {
            return None;
        };
        if slot.generation != token.generation() {
            return None;
        }
        cell.set(OpSlot {
            state: State::Free,
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
            State::Waiting { opens } => State::Orphan { opens },
            State::Done {
                status,
                value,
                opens: true,
            } => self.close_orphan(token, status.result().map(|()| value)),
            _ => State::Free,
        };
        cell.set(slot);
    }

    fn cancel(&self, token: Token) {
        let cancel = Sqe::new(token, RawHandle::new(0, 0), &Op::Cancel, Span::default());
        let _ = self.0.sq.write(&cancel);
    }

    /// Ops not yet taken, or orphaned and not yet landed.
    pub fn in_flight(&self) -> usize {
        self.pump();
        let used = |s: &&Cell<OpSlot>| !matches!(s.get().state, State::Free);
        self.0.ops.iter().filter(used).count()
    }
}

enum Stage {
    Queued(Token),
    /// Finished without reaching the queue, such as a `QueueFull`.
    Local(IoStatus),
    Taken,
}

/// An op in flight.
///
/// Poll it once per cycle, or await it in a sequence. Dropping it leaves the
/// op running and discards its result; [`Pending::cancel`] cancels it.
#[must_use]
pub struct Pending<O: IoOp> {
    io: Io,
    state: Stage,
    buf: Option<Buf>,
    _o: PhantomData<fn() -> O>,
}

impl<O: IoOp> Pending<O> {
    /// The result once the op has completed. `None` before that, and after
    /// the result was taken.
    pub fn poll(&mut self) -> Option<O::Output> {
        let (status, value) = match self.state {
            Stage::Queued(token) => self.io.take(token)?,
            Stage::Local(status) => (status, 0),
            Stage::Taken => return None,
        };
        self.state = Stage::Taken;
        Some(O::finish(&self.io, status, value, self.buf.take()))
    }

    /// The result if the op has completed, or the op back.
    pub fn try_take(mut self) -> Result<O::Output, Self> {
        self.poll().ok_or(self)
    }

    /// Cancels the op. Its completion still arrives, usually `Canceled`. The
    /// cancel takes no slot of its own.
    pub fn cancel(&self) {
        if let Stage::Queued(token) = self.state {
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
        if let Stage::Queued(token) = self.state {
            self.io.orphan(token);
        }
    }
}

mod sealed {
    pub trait Sealed {}
}

/// How an op's completion becomes the value its `Pending` resolves to.
pub trait IoOp: sealed::Sealed {
    type Output;

    #[doc(hidden)]
    fn finish(io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output;
}

/// An op that lends a buffer and hands it back with its result.
pub trait BufOp: IoOp<Output = (Result<Self::Ok, IoError>, Buf)> {
    type Ok;
}

/// Fills a buffer; resolves to the bytes read.
pub struct Read;
/// Sends a buffer's bytes; resolves to the bytes written.
pub struct Write;
/// Receives one message into a buffer.
pub struct Recv;
/// Sends one message from a buffer.
pub struct Send;
/// Opens a file under a directory.
pub struct Open;
/// Accepts a connection on a listener.
pub struct Accept;
/// Closes a handle.
pub struct Close;
/// Flushes a file to storage.
pub struct Fsync;
/// Configures or flushes a serial port.
pub struct Tty;
/// Reads or sets a GPIO line; resolves to the line's level.
pub struct Gpio;
/// Any op, with its raw result.
pub struct Raw;

/// The buffer back, with the length the op reports. A fill sets the
/// buffer's length to it.
fn moved(
    status: IoStatus,
    value: u32,
    buf: Option<Buf>,
    fill: bool,
) -> (Result<usize, IoError>, Buf) {
    let mut buf = buf.unwrap_or_else(Buf::empty);
    let result = status.result().map(|()| value as usize);
    if let (Ok(n), true) = (result, fill) {
        buf.set_len(n);
    }
    (result, buf)
}

fn opened<K>(io: &Io, status: IoStatus, value: u32) -> Result<Handle<K>, IoError> {
    status.result()?;
    Ok(Handle::new(RawHandle::from_raw(value), io.clone()))
}

impl sealed::Sealed for Read {}

impl IoOp for Read {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, true)
    }
}

impl sealed::Sealed for Recv {}

impl IoOp for Recv {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, true)
    }
}

impl sealed::Sealed for Write {}

impl IoOp for Write {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, false)
    }
}

impl sealed::Sealed for Send {}

impl IoOp for Send {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, false)
    }
}

impl sealed::Sealed for Open {}

impl IoOp for Open {
    type Output = Result<Handle<File>, IoError>;

    fn finish(io: &Io, status: IoStatus, value: u32, _buf: Option<Buf>) -> Self::Output {
        opened(io, status, value)
    }
}

impl sealed::Sealed for Accept {}

impl IoOp for Accept {
    type Output = Result<Handle<Stream>, IoError>;

    fn finish(io: &Io, status: IoStatus, value: u32, _buf: Option<Buf>) -> Self::Output {
        opened(io, status, value)
    }
}

impl sealed::Sealed for Close {}

impl IoOp for Close {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for Fsync {}

impl IoOp for Fsync {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for Tty {}

impl IoOp for Tty {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for Gpio {}

impl IoOp for Gpio {
    type Output = Result<u32, IoError>;

    fn finish(_io: &Io, status: IoStatus, value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result().map(|()| value)
    }
}

impl sealed::Sealed for Raw {}

impl IoOp for Raw {
    type Output = (Result<u32, IoError>, Option<Buf>);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        (status.result().map(|()| value), buf)
    }
}

impl BufOp for Read {
    type Ok = usize;
}
impl BufOp for Recv {
    type Ok = usize;
}
impl BufOp for Write {
    type Ok = usize;
}
impl BufOp for Send {
    type Ok = usize;
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
