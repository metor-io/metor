//! `Io`, a system's queue pair: ops go out on `io_sq` and completions come
//! back on `io_cq`.

use core::cell::Cell;
use core::ops::{Deref, DerefMut};
use std::rc::Rc;

use crate::port::{Input, Output};

use super::pending::{Inner, Pending, Raw};
use super::slot::Buf;
use super::{BufError, BufTable, Cqe, IoError, IoStatus, MAX_OPS, Op, Out, RawHandle, Sqe, Token};

/// The buffer slots one system's table holds.
pub(crate) const BUF_SLOTS: usize = 64;

/// A system's buffer table, and the bytes its constructor asked for and did
/// not get.
pub(crate) struct Table {
    pub(crate) bufs: Rc<BufTable>,
    /// Set while a constructor runs, when `alloc` may move the arena.
    pub(crate) making: Cell<bool>,
    shortfall: Cell<usize>,
    refusal: Cell<Option<BufError>>,
}

impl Table {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            bufs: Rc::new(BufTable::new(BUF_SLOTS)),
            making: Cell::new(false),
            shortfall: Cell::new(0),
            refusal: Cell::new(None),
        })
    }

    /// Allocates `len` bytes, or records the shortfall.
    pub(crate) fn alloc(&self, len: usize) -> Option<u16> {
        self.bufs
            .alloc(len)
            .inspect_err(|e| self.refuse(len, *e))
            .ok()
    }

    pub(crate) fn alloc_from(&self, bytes: &[u8]) -> Option<u16> {
        self.bufs
            .alloc_from(bytes)
            .inspect_err(|e| self.refuse(bytes.len(), *e))
            .ok()
    }

    fn refuse(&self, len: usize, e: BufError) {
        self.shortfall.set(self.shortfall.get() + len);
        if self.refusal.get().is_none() {
            self.refusal.set(Some(e));
        }
    }

    /// The bytes refused since the last call, and the first reason.
    pub(crate) fn take_shortfall(&self) -> Option<(usize, BufError)> {
        let need = self.shortfall.take();
        self.refusal.take().map(|e| (need, e))
    }
}

/// The token of an op whose completion no slot waits for. Its slot index is
/// past every slot, so the completion is skipped.
const UNTRACKED: Token = Token::new(u32::MAX);

/// Encodes a slot and its generation as a token.
fn token(slot: usize, generation: u16) -> Token {
    Token::new(u32::from(generation) << 16 | slot as u32)
}

fn split(token: Token) -> (usize, u16) {
    let t = token.get();
    ((t & 0xFFFF) as usize, (t >> 16) as u16)
}

enum OpSlot<B> {
    Free,
    Waiting(B),
    /// Its `Pending` was dropped; the completion frees it.
    Orphan(B),
    Done(IoStatus, Out, B),
}

struct Entry<B> {
    generation: u16,
    op: OpSlot<B>,
}

/// The ops a system has in flight, each holding a payload `B` until it
/// completes.
pub(crate) struct Slots<B> {
    entries: Box<[Entry<B>]>,
    generation: u16,
}

impl<B> Slots<B> {
    pub(crate) fn new() -> Self {
        let free = || Entry {
            generation: 0,
            op: OpSlot::Free,
        };
        Self {
            entries: (0..MAX_OPS).map(|_| free()).collect(),
            generation: 0,
        }
    }

    /// Takes a free slot for an op holding `payload`. Hands the payload back
    /// when every slot is taken.
    pub(crate) fn claim(&mut self, payload: B) -> Result<Token, B> {
        let Some(slot) = self
            .entries
            .iter()
            .position(|e| matches!(e.op, OpSlot::Free))
        else {
            return Err(payload);
        };
        self.generation = self.generation.wrapping_add(1);
        self.entries[slot] = Entry {
            generation: self.generation,
            op: OpSlot::Waiting(payload),
        };
        Ok(token(slot, self.generation))
    }

    fn entry(&mut self, token: Token) -> Option<&mut OpSlot<B>> {
        let (slot, generation) = split(token);
        let entry = self.entries.get_mut(slot)?;
        (entry.generation == generation).then_some(&mut entry.op)
    }

    /// Frees a claimed slot whose op never went out, and returns its payload.
    pub(crate) fn unclaim(&mut self, token: Token) -> Option<B> {
        let op = self.entry(token)?;
        if !matches!(op, OpSlot::Waiting(_)) {
            return None;
        }
        match core::mem::replace(op, OpSlot::Free) {
            OpSlot::Waiting(b) => Some(b),
            _ => None,
        }
    }

    /// Records a completion. Skips one for a free slot or an old generation.
    ///
    /// Returns the handle an orphaned op opened, which no one will close.
    pub(crate) fn land(&mut self, cqe: &Cqe) -> Option<RawHandle> {
        let op = self.entry(cqe.token)?;
        let (next, opened) = match core::mem::replace(op, OpSlot::Free) {
            OpSlot::Waiting(b) => (OpSlot::Done(cqe.status, cqe.out, b), None),
            OpSlot::Orphan(_) => (OpSlot::Free, opened(cqe)),
            done @ OpSlot::Done(..) => (done, None),
            OpSlot::Free => (OpSlot::Free, None),
        };
        *op = next;
        opened
    }

    /// Takes a finished op's result, freeing its slot.
    pub(crate) fn take(&mut self, token: Token) -> Option<(IoStatus, Out, B)> {
        let op = self.entry(token)?;
        if !matches!(op, OpSlot::Done(..)) {
            return None;
        }
        match core::mem::replace(op, OpSlot::Free) {
            OpSlot::Done(status, out, b) => Some((status, out, b)),
            _ => None,
        }
    }

    /// Drops interest in an op. A finished op is freed now, and a waiting one
    /// when its completion lands.
    pub(crate) fn orphan(&mut self, token: Token) {
        let Some(op) = self.entry(token) else {
            return;
        };
        *op = match core::mem::replace(op, OpSlot::Free) {
            OpSlot::Waiting(b) => OpSlot::Orphan(b),
            _ => OpSlot::Free,
        };
    }

    /// The slots in use.
    pub(crate) fn live(&self) -> usize {
        let used = |e: &&Entry<B>| !matches!(e.op, OpSlot::Free);
        self.entries.iter().filter(used).count()
    }
}

/// The handle a successful completion carries, if any.
fn opened(cqe: &Cqe) -> Option<RawHandle> {
    cqe.status.result().ok().and(cqe.out.as_handle())
}

struct State {
    cq: Input<Cqe>,
    slots: Slots<Option<Buf>>,
    next_grant: u32,
}

impl State {
    /// Records every completion on `cq`, and closes any handle an orphaned
    /// op opened. Skips records that do not decode.
    fn pump(&mut self, sq: &Output<Sqe>) {
        for cqe in self.cq.drain().flatten() {
            if let Some(raw) = self.slots.land(&cqe) {
                let _ = sq.write(&Sqe::new(UNTRACKED, raw, Op::CLOSE));
            }
        }
    }
}

struct Queue {
    sq: Output<Sqe>,
    state: Cell<Option<Box<State>>>,
    table: Rc<Table>,
}

/// A system's queue pair and buffer table, shared by `Rc`.
///
/// Handles, pending ops, and buffers each hold one.
#[derive(Clone)]
pub struct Io(Rc<Queue>);

/// The queue's state, borrowed for one call and put back on drop.
struct Lent<'a> {
    cell: &'a Cell<Option<Box<State>>>,
    state: Option<Box<State>>,
}

impl Deref for Lent<'_> {
    type Target = State;

    fn deref(&self) -> &State {
        // PANIC Safety: `state` is `Some` until `drop`.
        self.state.as_deref().expect("lent state")
    }
}

impl DerefMut for Lent<'_> {
    fn deref_mut(&mut self) -> &mut State {
        // PANIC Safety: `state` is `Some` until `drop`.
        self.state.as_deref_mut().expect("lent state")
    }
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        self.cell.set(self.state.take());
    }
}

impl Io {
    /// A queue that writes ops to `sq` and reads completions from `cq`.
    ///
    /// A full `sq` fails the op with `QueueFull` and counts no dropped write.
    pub(crate) fn new(sq: Output<Sqe>, cq: Input<Cqe>, table: Rc<Table>) -> Self {
        let state = State {
            cq,
            slots: Slots::new(),
            next_grant: 0,
        };
        Self(Rc::new(Queue {
            sq,
            state: Cell::new(Some(Box::new(state))),
            table,
        }))
    }

    pub(crate) fn table(&self) -> &Rc<Table> {
        &self.0.table
    }

    /// `None` while another call holds the state.
    fn lend(&self) -> Option<Lent<'_>> {
        let state = self.0.state.take()?;
        Some(Lent {
            cell: &self.0.state,
            state: Some(state),
        })
    }

    /// Writes an op and claims a slot for it. A full queue completes the op
    /// with `QueueFull` on the spot, handing `buf` back.
    pub(crate) fn submit(&self, raw: RawHandle, op: Op, buf: Option<Buf>) -> Inner {
        let full = |buf| Inner::Local(IoStatus::err(IoError::QueueFull), buf);
        let Some(op) = lent(op, buf.as_ref()) else {
            return Inner::Local(IoStatus::err(IoError::BadBuf), buf);
        };
        let Some(mut state) = self.lend() else {
            return full(buf);
        };
        state.pump(&self.0.sq);
        let token = match state.slots.claim(buf) {
            Ok(token) => token,
            Err(buf) => return full(buf),
        };
        if self.0.sq.write(&Sqe::new(token, raw, op)).is_err() {
            return full(state.slots.unclaim(token).flatten());
        }
        Inner::Queued(token)
    }

    /// Takes a finished op's result.
    pub(crate) fn take(&self, token: Token) -> Option<(IoStatus, Out, Option<Buf>)> {
        let mut state = self.lend()?;
        state.pump(&self.0.sq);
        state.slots.take(token)
    }

    /// Writes an op whose result no one takes, such as a cancel. It needs no
    /// slot, so it goes out even when every slot is in use.
    pub(crate) fn untracked(&self, raw: RawHandle, op: Op) {
        let _ = self.0.sq.write(&Sqe::new(UNTRACKED, raw, op));
    }

    pub(crate) fn orphan(&self, token: Token) {
        if let Some(mut state) = self.lend() {
            state.slots.orphan(token);
        }
    }

    /// The handle of the next `Cap` this queue binds: grant `n` is index `n`.
    pub(crate) fn next_grant(&self) -> RawHandle {
        let Some(mut state) = self.lend() else {
            return RawHandle::new(u32::MAX, 0);
        };
        state.next_grant += 1;
        RawHandle::new(state.next_grant - 1, 0)
    }

    /// Submits any op on any handle, lending it `buf`. Most systems use a
    /// `Handle` instead.
    pub fn submit_raw(&self, raw: RawHandle, op: Op, buf: Option<Buf>) -> Pending<Raw> {
        Pending::new(self, raw, op, buf)
    }

    /// Ops that have not been taken or orphaned and freed.
    pub fn in_flight(&self) -> usize {
        self.lend().map_or(0, |mut state| {
            state.pump(&self.0.sq);
            state.slots.live()
        })
    }
}

/// `op` naming the buffer it lends. An op that takes a buffer and lends none
/// is refused.
fn lent(mut op: Op, buf: Option<&Buf>) -> Option<Op> {
    match (op.buf_mut(), buf) {
        (Some(slot), Some(buf)) => *slot = buf.buf_ref(),
        (Some(_), None) => return None,
        (None, _) => {}
    }
    Some(op)
}
