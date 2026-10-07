//! A slot-based allocator for IO buffers, backed by a single allocation.
//!
//! The allocation starts with a directory: a header `{ slots, used, top }`
//! followed by `slots` entries `{ offset, cap, len, held }`, each a
//! little-endian `u32`. Buffer data follows the directory. Before `freeze`,
//! the allocation is a `Vec` that `alloc` grows. `freeze` fixes its size and
//! address, and only then does `raw` return pointers. After `freeze`, the only
//! `&mut` into the allocation covers the directory or comes from the unsafe
//! accessors, so pointers held by the kernel stay valid.

use core::cell::{Cell, RefCell};
use core::ptr::NonNull;
use std::rc::Rc;

use zerocopy::little_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// The index of a `BufTable` in its `Tables`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TableId(pub(crate) u16);

/// A reference to a buffer slot.
///
/// Systems set only the slot. The reactor fills in the table.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct BufRef {
    table: U16,
    slot: U16,
}

impl BufRef {
    pub const fn new(slot: u16) -> Self {
        Self {
            table: U16::ZERO,
            slot: U16::new(slot),
        }
    }

    pub fn slot(self) -> u16 {
        self.slot.get()
    }

    pub fn table(self) -> TableId {
        TableId(self.table.get())
    }

    pub(crate) fn stamp(&mut self, table: TableId) {
        self.table = U16::new(table.0);
    }
}

/// A pointer to a slot's memory, with its capacity and filled length.
#[derive(Clone, Copy, Debug)]
pub struct RawBuf {
    pub ptr: NonNull<u8>,
    pub cap: u32,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BufError {
    #[error("every slot is held")]
    Full,
    #[error("longer than the buffer")]
    TooLarge,
    #[error("no such slot")]
    BadSlot,
    #[error("needs {need} bytes, {free} free")]
    NoSpace { need: usize, free: usize },
}

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct Header {
    slots: U32,
    used: U32,
    top: U32,
}

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct Entry {
    offset: U32,
    cap: U32,
    len: U32,
    held: U32,
}

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct Dir {
    header: Header,
    entries: [Entry],
}

impl Dir {
    fn held(&self, slot: u16) -> Option<&Entry> {
        self.entries
            .get(usize::from(slot))
            .filter(|e| e.held.get() != 0)
    }

    fn held_mut(&mut self, slot: u16) -> Option<&mut Entry> {
        self.entries
            .get_mut(usize::from(slot))
            .filter(|e| e.held.get() != 0)
    }
}

fn dir_len(slots: u16) -> usize {
    size_of::<Header>() + usize::from(slots) * size_of::<Entry>()
}

enum Arena {
    Making(Vec<u8>),
    Frozen(NonNull<[u8]>),
}

impl Arena {
    fn base(&mut self) -> NonNull<u8> {
        match self {
            Self::Making(v) => NonNull::new(v.as_mut_ptr()).unwrap_or(NonNull::dangling()),
            Self::Frozen(p) => p.cast(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Making(v) => v.len(),
            Self::Frozen(p) => p.len(),
        }
    }

    /// Borrows the directory without touching buffer data.
    fn dir(&mut self, slots: u16) -> &mut Dir {
        let n = dir_len(slots);
        // `base` forms no reference to the data, so borrows of buffers
        // elsewhere in the arena stay valid.
        let base = self.base();
        // SAFETY: the first `n` bytes hold the directory, and no `RawBuf` or
        // accessor overlaps them.
        let bytes = unsafe { core::slice::from_raw_parts_mut(base.as_ptr(), n) };
        // PANIC Safety: `bytes` holds exactly a header and `slots` entries.
        Dir::mut_from_bytes(bytes).expect("directory layout")
    }
}

/// A system's IO buffers. Shared through `Rc`, so every method takes `&self`.
pub struct BufTable {
    arena: RefCell<Arena>,
    slots: u16,
    id: Cell<Option<TableId>>,
}

impl BufTable {
    /// Creates a table with `slots` directory entries, capped at `u16::MAX`.
    pub fn new(slots: usize) -> Self {
        let slots = u16::try_from(slots).unwrap_or(u16::MAX);
        let mut arena = Arena::Making(vec![0; dir_len(slots)]);
        let header = &mut arena.dir(slots).header;
        header.slots = U32::new(u32::from(slots));
        header.top = U32::new(dir_len(slots) as u32);
        Self {
            arena: RefCell::new(arena),
            slots,
            id: Cell::new(None),
        }
    }

    /// The table's index in its `Tables`, once added.
    pub fn id(&self) -> Option<TableId> {
        self.id.get()
    }

    /// Allocates a zeroed buffer of `cap` bytes. Before `freeze` the allocation
    /// grows to fit. After `freeze` the buffer must fit in the remaining space.
    pub fn alloc(&self, cap: usize) -> Result<u16, BufError> {
        let cap32 = u32::try_from(cap).map_err(|_| BufError::TooLarge)?;
        let mut arena = self.arena.borrow_mut();
        let dir = arena.dir(self.slots);
        let slot = dir
            .entries
            .iter()
            .position(|e| e.held.get() == 0)
            .ok_or(BufError::Full)?;
        let top = dir.header.top.get() as usize;
        grow(&mut arena, top, cap)?;
        let dir = arena.dir(self.slots);
        dir.entries[slot] = Entry {
            offset: U32::new(top as u32),
            cap: U32::new(cap32),
            len: U32::ZERO,
            held: U32::new(1),
        };
        dir.header.top = U32::new((top + cap) as u32);
        dir.header.used = U32::new(dir.header.used.get() + 1);
        // PANIC Safety: `entries` holds at most `u16::MAX` items.
        Ok(u16::try_from(slot).expect("slot fits u16"))
    }

    /// Allocates a buffer filled with `bytes`.
    pub fn alloc_from(&self, bytes: &[u8]) -> Result<u16, BufError> {
        let slot = self.alloc(bytes.len())?;
        let mut arena = self.arena.borrow_mut();
        let entry = &mut arena.dir(self.slots).entries[usize::from(slot)];
        entry.len = entry.cap;
        let offset = entry.offset.get() as usize;
        // SAFETY: the slot was just allocated with `bytes.len()` bytes at `offset`.
        unsafe {
            let dst = arena.base().as_ptr().add(offset);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
        Ok(slot)
    }

    /// Fixes the allocation's size and address. Later calls do nothing.
    pub fn freeze(&self) {
        let mut arena = self.arena.borrow_mut();
        if let Arena::Making(v) = &mut *arena {
            let boxed = core::mem::take(v).into_boxed_slice();
            *arena = Arena::Frozen(NonNull::from(Box::leak(boxed)));
        }
    }

    /// The whole allocation, once frozen.
    pub fn arena(&self) -> Option<NonNull<[u8]>> {
        match *self.arena.borrow() {
            Arena::Frozen(p) => Some(p),
            Arena::Making(_) => None,
        }
    }

    /// Frees a slot. Its bytes stay unused until a rewind.
    pub fn release(&self, slot: u16) {
        let mut arena = self.arena.borrow_mut();
        let dir = arena.dir(self.slots);
        if let Some(e) = dir.held_mut(slot) {
            e.held = U32::ZERO;
            dir.header.used = U32::new(dir.header.used.get() - 1);
        }
    }

    /// A pointer to the slot's memory, or `None` before `freeze`.
    pub fn raw(&self, slot: u16) -> Option<RawBuf> {
        let mut arena = self.arena.borrow_mut();
        let Arena::Frozen(base) = *arena else {
            return None;
        };
        let e = arena.dir(self.slots).held(slot)?;
        // SAFETY: a held entry lies inside the allocation.
        let ptr = unsafe { base.cast::<u8>().add(e.offset.get() as usize) };
        Some(RawBuf {
            ptr,
            cap: e.cap.get(),
            len: e.len.get(),
        })
    }

    /// The slot's capacity, or 0 for a free slot.
    pub fn capacity(&self, slot: u16) -> u32 {
        let mut arena = self.arena.borrow_mut();
        let dir = arena.dir(self.slots);
        dir.held(slot).map_or(0, |e| e.cap.get())
    }

    pub fn set_len(&self, slot: u16, len: u32) -> Result<(), BufError> {
        let mut arena = self.arena.borrow_mut();
        let e = arena
            .dir(self.slots)
            .held_mut(slot)
            .ok_or(BufError::BadSlot)?;
        if len > e.cap.get() {
            return Err(BufError::TooLarge);
        }
        e.len = U32::new(len);
        Ok(())
    }

    /// The slot's filled bytes, or an empty slice for a free slot.
    ///
    /// # Safety
    /// The caller owns the slot and has no op in flight on it. Before `freeze`,
    /// the caller must not call `alloc` while the slice is alive.
    pub unsafe fn filled(&self, slot: u16) -> &[u8] {
        // SAFETY: the caller upholds the contract.
        unsafe { self.filled_mut(slot) }
    }

    /// # Safety
    /// Same as [`BufTable::filled`].
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn filled_mut(&self, slot: u16) -> &mut [u8] {
        // SAFETY: the caller upholds the contract.
        unsafe { self.span(slot, |e| e.len.get()) }
    }

    /// The slot's full capacity, filled or not.
    ///
    /// # Safety
    /// Same as [`BufTable::filled`].
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn whole_mut(&self, slot: u16) -> &mut [u8] {
        // SAFETY: the caller upholds the contract.
        unsafe { self.span(slot, |e| e.cap.get()) }
    }

    /// # Safety
    /// Same as [`BufTable::filled`].
    #[allow(clippy::mut_from_ref)]
    unsafe fn span(&self, slot: u16, len: impl Fn(&Entry) -> u32) -> &mut [u8] {
        let mut arena = self.arena.borrow_mut();
        let Some(e) = arena.dir(self.slots).held(slot) else {
            return &mut [];
        };
        let (offset, len) = (e.offset.get() as usize, len(e) as usize);
        let base = arena.base();
        // SAFETY: a held entry lies inside the allocation, and the caller owns the slot.
        unsafe { core::slice::from_raw_parts_mut(base.as_ptr().add(offset), len) }
    }
}

/// Makes room for `cap` bytes at `top`.
fn grow(arena: &mut Arena, top: usize, cap: usize) -> Result<(), BufError> {
    let free = arena.len() - top;
    if top + cap > u32::MAX as usize {
        return Err(BufError::TooLarge);
    }
    match arena {
        Arena::Making(v) => v.resize(top + cap, 0),
        Arena::Frozen(_) if cap > free => return Err(BufError::NoSpace { need: cap, free }),
        // SAFETY: `top..top + cap` lies inside the allocation, past every held slot.
        Arena::Frozen(p) => unsafe { p.cast::<u8>().add(top).write_bytes(0, cap) },
    }
    Ok(())
}

impl Drop for BufTable {
    fn drop(&mut self) {
        if let Arena::Frozen(p) = *self.arena.get_mut() {
            // SAFETY: `p` came from `Box::leak` in `freeze` and is freed once.
            drop(unsafe { Box::from_raw(p.as_ptr()) });
        }
    }
}

/// The buffer tables of every system on a target. Backends resolve `BufRef`s
/// through it.
#[derive(Clone, Default)]
pub struct Tables(Rc<RefCell<Vec<Rc<BufTable>>>>);

impl Tables {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a table and sets its id. Add each table to only one `Tables`.
    pub fn add(&self, table: &Rc<BufTable>) -> TableId {
        debug_assert!(table.id().is_none(), "table already added");
        let mut tables = self.0.borrow_mut();
        // PANIC Safety: a target has far fewer than 65536 systems.
        let id = TableId(u16::try_from(tables.len()).expect("table count fits u16"));
        table.id.set(Some(id));
        tables.push(table.clone());
        id
    }

    pub fn len(&self) -> usize {
        self.0.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }

    pub fn get(&self, id: TableId) -> Option<Rc<BufTable>> {
        self.0.borrow().get(usize::from(id.0)).cloned()
    }

    pub fn raw(&self, buf: BufRef) -> Option<RawBuf> {
        self.0
            .borrow()
            .get(usize::from(buf.table().0))?
            .raw(buf.slot())
    }

    pub fn set_len(&self, buf: BufRef, len: u32) -> Result<(), BufError> {
        let tables = self.0.borrow();
        let table = tables
            .get(usize::from(buf.table().0))
            .ok_or(BufError::BadSlot)?;
        table.set_len(buf.slot(), len)
    }
}
