//! `Buf`, a slot in a system's buffer table; `Slot`, which keeps one at home
//! in state; and `IoInit`, which allocates them while a system is made.

use core::cell::Cell;
use core::ops::{Deref, DerefMut};
use std::rc::Rc;

use crate::coordinator::ParamError;

use super::pending::{BufOp, Pending};
use super::queue::{Io, Table};
use super::{BufRef, IoError};

/// A buffer in its system's buffer table.
///
/// It derefs to its filled bytes. An op takes it by value and hands it back
/// with the result. Dropping it frees the slot.
pub struct Buf {
    table: Option<Rc<Table>>,
    slot: u16,
}

impl Buf {
    /// A buffer of no capacity, in no table.
    pub(crate) fn empty() -> Self {
        Self {
            table: None,
            slot: u16::MAX,
        }
    }

    pub(crate) fn new(table: &Rc<Table>, slot: Option<u16>) -> Self {
        match slot {
            Some(slot) => Self {
                table: Some(table.clone()),
                slot,
            },
            None => Self::empty(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.table
            .as_ref()
            .map_or(0, |t| t.bufs.capacity(self.slot) as usize)
    }

    /// Copies as much of `bytes` as fits and sets the length to it.
    pub fn fill(&mut self, bytes: &[u8]) -> usize {
        let Some(table) = &self.table else {
            return 0;
        };
        // SAFETY: this `Buf` owns the slot, and the borrow ends in this call.
        let whole = unsafe { table.bufs.whole_mut(self.slot) };
        let n = bytes.len().min(whole.len());
        whole[..n].copy_from_slice(&bytes[..n]);
        self.set_len(n);
        n
    }

    pub fn clear(&mut self) {
        self.set_len(0);
    }

    /// Sets the filled length, clamped to the capacity.
    pub fn set_len(&mut self, n: usize) {
        if let Some(table) = &self.table {
            let n = n.min(self.capacity()) as u32;
            let _ = table.bufs.set_len(self.slot, n);
        }
    }

    pub(crate) fn buf_ref(&self) -> BufRef {
        BufRef::new(self.slot)
    }

    /// The table, unless it is being made and its arena may still move.
    fn settled(&self) -> Option<&Table> {
        self.table.as_deref().filter(|t| !t.making.get())
    }
}

impl Deref for Buf {
    type Target = [u8];

    /// The filled bytes. Empty while the system's constructor runs.
    fn deref(&self) -> &[u8] {
        match self.settled() {
            // SAFETY: only this `Buf` reaches its slot. While an op holds the
            // slot, the `Buf` sits in the queue. The arena does not move
            // outside a constructor, which `settled` rules out.
            Some(table) => unsafe { table.bufs.filled(self.slot) },
            None => &[],
        }
    }
}

impl DerefMut for Buf {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self.settled() {
            // SAFETY: as in `deref`, and `&mut self` makes the borrow unique.
            Some(table) => unsafe { table.bufs.filled_mut(self.slot) },
            None => &mut [],
        }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        if let Some(table) = &self.table {
            table.bufs.release(self.slot);
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

/// Allocates a system's buffers while its constructor runs.
pub struct IoInit<'a> {
    table: Option<&'a Rc<Table>>,
    /// Bytes asked of a system with no table.
    orphaned: Cell<usize>,
}

impl<'a> IoInit<'a> {
    pub(crate) fn new(io: &'a Io) -> Self {
        io.table().making.set(true);
        Self {
            table: Some(io.table()),
            orphaned: Cell::new(0),
        }
    }

    /// An `IoInit` for a system with no queue. Every allocation fails the make.
    pub(crate) fn empty() -> Self {
        Self {
            table: None,
            orphaned: Cell::new(0),
        }
    }

    /// Allocates `len` zeroed bytes. If the table cannot, returns an empty
    /// buffer and the make fails afterward.
    pub fn alloc(&self, len: usize) -> Buf {
        match self.table {
            Some(table) => Buf::new(table, table.alloc(len)),
            None => self.orphan(len),
        }
    }

    /// Allocates a buffer filled with `bytes`.
    pub fn alloc_from(&self, bytes: &[u8]) -> Buf {
        match self.table {
            Some(table) => Buf::new(table, table.alloc_from(bytes)),
            None => self.orphan(bytes.len()),
        }
    }

    fn orphan(&self, len: usize) -> Buf {
        self.orphaned.set(self.orphaned.get() + len);
        Buf::empty()
    }

    /// Fails with `IoMemory` if any allocation was refused.
    pub(crate) fn finish(self) -> Result<(), ParamError> {
        let orphaned = self.orphaned.get();
        if orphaned > 0 {
            let reason =
                format!("needs {orphaned} bytes, but the system has no `Cap` or `DeviceSide`");
            return Err(ParamError::IoMemory {
                need: orphaned,
                reason,
            });
        }
        match self.table.and_then(|t| t.take_shortfall()) {
            Some((need, e)) => Err(ParamError::IoMemory {
                need,
                reason: e.to_string(),
            }),
            None => Ok(()),
        }
    }
}

impl Drop for IoInit<'_> {
    /// Freezes the table, so its arena never moves while a `Buf` is read.
    fn drop(&mut self) {
        if let Some(table) = self.table {
            table.bufs.freeze();
            table.making.set(false);
        }
    }
}
