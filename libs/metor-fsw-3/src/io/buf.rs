//! A slot-based allocator for IO buffers, backed by a single allocation.
//!
//! The allocation starts with a directory: a header `{ slots, used, top }`
//! followed by `slots` entries `{ offset, cap, len, held }`, each a
//! little-endian `u32`. Buffer data follows the directory. Before `freeze`,
//! the allocation is a `Vec` that `alloc` grows. `freeze` fixes its size and
//! address, and only then does `raw` return pointers. After `freeze`, the only
//! `&mut` into the allocation covers the directory or comes from the unsafe
//! accessors, so pointers held by the kernel stay valid.
//!
//! A table can also sit over a region another party laid out, such as a
//! guest's arena. Either side may change the directory, so every entry is
//! checked against the region each time it is read.

use core::cell::{Cell, RefCell};
use core::ffi::c_void;
use core::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use metor_fsw_3_ring::RawOwner;

use zerocopy::little_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::region::Region;

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
    #[error("a slot is still held")]
    Busy,
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

impl Entry {
    /// Whether the entry is held and lies inside an arena of `limit` bytes,
    /// past a directory of `dir` bytes.
    fn valid(&self, dir: usize, limit: usize) -> bool {
        let (offset, cap) = (self.offset.get() as usize, self.cap.get() as usize);
        self.held.get() != 0
            && offset >= dir
            && offset.checked_add(cap).is_some_and(|end| end <= limit)
            && self.len.get() <= self.cap.get()
    }
}

impl Dir {
    /// The held entry for `slot`, if it is valid in an arena of `limit`
    /// bytes.
    fn held(&self, slot: u16, limit: usize) -> Option<&Entry> {
        let dir = dir_len(self.entries.len() as u16);
        let entry = self.entries.get(usize::from(slot))?;
        entry.valid(dir, limit).then_some(entry)
    }

    fn held_mut(&mut self, slot: u16, limit: usize) -> Option<&mut Entry> {
        let dir = dir_len(self.entries.len() as u16);
        let entry = self.entries.get_mut(usize::from(slot))?;
        entry.valid(dir, limit).then_some(entry)
    }
}

/// A region whose directory does not fit or names memory outside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a buffer table's directory is corrupt")]
pub struct Corrupt;

fn dir_len(slots: u16) -> usize {
    size_of::<Header>() + usize::from(slots) * size_of::<Entry>()
}

enum Arena {
    Making(Vec<u8>),
    Frozen(Arc<Mem>),
    /// Memory another party owns or this side mirrors. Its size is fixed.
    Region(Region),
}

impl Arena {
    fn base(&mut self) -> NonNull<u8> {
        match self {
            Self::Making(v) => NonNull::new(v.as_mut_ptr()).unwrap_or(NonNull::dangling()),
            Self::Frozen(m) => m.0.cast(),
            Self::Region(r) => r.base(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Making(v) => v.len(),
            Self::Frozen(m) => m.0.len(),
            Self::Region(r) => r.len(),
        }
    }

    /// The fixed memory, or `None` while a constructor may still grow it.
    fn fixed(&mut self) -> Option<NonNull<[u8]>> {
        match self {
            Self::Making(_) => None,
            Self::Frozen(m) => Some(m.0),
            Self::Region(r) => Some(NonNull::slice_from_raw_parts(r.base(), r.len())),
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

    /// A table over a region laid out elsewhere. It is frozen: its size never
    /// changes. A directory that does not fit the region, or a held entry
    /// outside it, is `Corrupt`.
    pub(crate) fn from_region(mut region: Region) -> Result<Self, Corrupt> {
        let len = region.len();
        let header_len = size_of::<Header>();
        if len < header_len {
            return Err(Corrupt);
        }
        // SAFETY: the region holds at least a header, and nothing else
        // borrows it yet.
        let head = unsafe { core::slice::from_raw_parts(region.base().as_ptr(), header_len) };
        let header = Header::ref_from_bytes(head).map_err(|_| Corrupt)?;
        let slots = u16::try_from(header.slots.get()).map_err(|_| Corrupt)?;
        if dir_len(slots) > len {
            return Err(Corrupt);
        }
        let mut arena = Arena::Region(region);
        let dir = arena.dir(slots);
        let bad = |e: &Entry| e.held.get() != 0 && !e.valid(dir_len(slots), len);
        if dir.entries.iter().any(bad) {
            return Err(Corrupt);
        }
        Ok(Self {
            arena: RefCell::new(arena),
            slots,
            id: Cell::new(None),
        })
    }

    /// The memory of the held slot `at` names, if it lies inside the arena.
    /// For a `BufRef` read from an untrusted op.
    pub fn checked(&self, at: BufRef) -> Option<NonNull<[u8]>> {
        let raw = self.raw(at.slot())?;
        Some(NonNull::slice_from_raw_parts(raw.ptr, raw.cap as usize))
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
        let top = (dir.header.top.get() as usize).max(dir_len(self.slots));
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
            *arena = Arena::Frozen(Arc::new(Mem(NonNull::from(Box::leak(boxed)))));
        }
    }

    /// The whole allocation, once frozen. It crosses the pack ABI as one
    /// region.
    pub fn region(&self) -> Option<NonNull<[u8]>> {
        self.arena.borrow_mut().fixed()
    }

    /// The fixed arena, and an owner that keeps it alive if it has one, to
    /// share with another party. `None` before `freeze`.
    ///
    /// The owner's context stays live while this table does.
    pub fn export(&self) -> Option<(NonNull<[u8]>, Option<RawOwner>)> {
        let mut arena = self.arena.borrow_mut();
        let span = arena.fixed()?;
        let owner = match &*arena {
            Arena::Frozen(mem) => Some(RawOwner {
                context: Arc::as_ptr(mem).cast(),
                retain: retain_mem,
                release: release_mem,
            }),
            Arena::Region(region) => region.owner(),
            Arena::Making(_) => None,
        };
        Some((span, owner))
    }

    /// Copies `bytes` over a mirror's region, which keeps its address. Fails
    /// for any other table, for a length other than the region's, or for a
    /// directory that does not fit; then the table is left as it was.
    pub(crate) fn refill(&self, bytes: &[u8]) -> Result<(), Corrupt> {
        let mut arena = self.arena.borrow_mut();
        let Arena::Region(Region::Mirror { bytes: mine }) = &mut *arena else {
            return Err(Corrupt);
        };
        if mine.len() != bytes.len() {
            return Err(Corrupt);
        }
        let fresh = BufTable::from_region(Region::mirror(bytes.into()))?;
        if fresh.slots != self.slots {
            return Err(Corrupt);
        }
        mine.copy_from_slice(bytes);
        Ok(())
    }

    /// Clears the directory, so the next allocations start again at the first
    /// data byte. The arena keeps its address and capacity. Fails with
    /// `Busy` while any slot is held.
    pub fn rewind(&self) -> Result<(), BufError> {
        let mut arena = self.arena.borrow_mut();
        let dir = arena.dir(self.slots);
        if dir.entries.iter().any(|e| e.held.get() != 0) {
            return Err(BufError::Busy);
        }
        dir.entries.iter_mut().for_each(|e| e.held = U32::ZERO);
        dir.header.used = U32::ZERO;
        dir.header.top = U32::new(dir_len(self.slots) as u32);
        Ok(())
    }

    /// Frees a slot. Its bytes stay unused until a rewind.
    pub fn release(&self, slot: u16) {
        let mut arena = self.arena.borrow_mut();
        let dir = arena.dir(self.slots);
        let held = dir.entries.get_mut(usize::from(slot));
        if let Some(e) = held.filter(|e| e.held.get() != 0) {
            e.held = U32::ZERO;
            dir.header.used = U32::new(dir.header.used.get() - 1);
        }
    }

    /// A pointer to the slot's memory, or `None` before `freeze`.
    pub fn raw(&self, slot: u16) -> Option<RawBuf> {
        let mut arena = self.arena.borrow_mut();
        let base = arena.fixed()?;
        let e = arena.dir(self.slots).held(slot, base.len())?;
        // SAFETY: a held entry lies inside the allocation.
        let ptr = unsafe { base.cast::<u8>().add(e.offset.get() as usize) };
        Some(RawBuf {
            ptr,
            cap: e.cap.get(),
            len: e.len.get(),
        })
    }

    /// A held slot's `(offset, cap, len)`, if it lies inside the arena.
    pub(crate) fn entry(&self, slot: u16) -> Option<(u32, u32, u32)> {
        let mut arena = self.arena.borrow_mut();
        let limit = arena.len();
        let e = arena.dir(self.slots).held(slot, limit)?;
        Some((e.offset.get(), e.cap.get(), e.len.get()))
    }

    /// The slot's capacity, or 0 for a free slot.
    pub fn capacity(&self, slot: u16) -> u32 {
        let mut arena = self.arena.borrow_mut();
        let limit = arena.len();
        let dir = arena.dir(self.slots);
        dir.held(slot, limit).map_or(0, |e| e.cap.get())
    }

    pub fn set_len(&self, slot: u16, len: u32) -> Result<(), BufError> {
        let mut arena = self.arena.borrow_mut();
        let limit = arena.len();
        let e = arena
            .dir(self.slots)
            .held_mut(slot, limit)
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
        let (ptr, len) = self.span(slot, |e| e.len.get());
        // SAFETY: the caller upholds the contract; a shared slice forms no
        // unique borrow, so two at once are fine.
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    /// # Safety
    /// Same as [`BufTable::filled`].
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn filled_mut(&self, slot: u16) -> &mut [u8] {
        let (ptr, len) = self.span(slot, |e| e.len.get());
        // SAFETY: the caller upholds the contract.
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }

    /// The slot's full capacity, filled or not.
    ///
    /// # Safety
    /// Same as [`BufTable::filled`].
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn whole_mut(&self, slot: u16) -> &mut [u8] {
        let (ptr, len) = self.span(slot, |e| e.cap.get());
        // SAFETY: the caller upholds the contract.
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }

    /// A held slot's start and `len(entry)`, or an empty span for a free slot.
    fn span(&self, slot: u16, len: impl Fn(&Entry) -> u32) -> (*mut u8, usize) {
        let mut arena = self.arena.borrow_mut();
        let limit = arena.len();
        let Some(e) = arena.dir(self.slots).held(slot, limit) else {
            return (NonNull::dangling().as_ptr(), 0);
        };
        let (offset, len) = (e.offset.get() as usize, len(e) as usize);
        // SAFETY: a held entry lies inside the allocation.
        (unsafe { arena.base().as_ptr().add(offset) }, len)
    }
}

/// Makes room for `cap` bytes at `top`.
fn grow(arena: &mut Arena, top: usize, cap: usize) -> Result<(), BufError> {
    let free = arena.len().saturating_sub(top);
    if top + cap > u32::MAX as usize {
        return Err(BufError::TooLarge);
    }
    if let Arena::Making(v) = arena {
        v.resize(top + cap, 0);
        return Ok(());
    }
    if cap > free {
        return Err(BufError::NoSpace { need: cap, free });
    }
    // SAFETY: `top..top + cap` lies inside the fixed arena, past every held
    // slot.
    unsafe { arena.base().as_ptr().add(top).write_bytes(0, cap) };
    Ok(())
}

/// A frozen arena's memory. Counted by `Arc`, so a party it is shared with
/// keeps it alive through a `RawOwner`.
struct Mem(NonNull<[u8]>);

// SAFETY: `Mem` only frees its memory. Access to the bytes goes through
// `BufTable`, where each slot belongs to one side at a time.
unsafe impl Send for Mem {}
unsafe impl Sync for Mem {}

impl Drop for Mem {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `Box::leak` in `freeze`, and the last
        // `Arc` frees it once.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

unsafe extern "C" fn retain_mem(context: *const c_void) {
    // SAFETY: `export` made `context` from a live `Arc<Mem>`.
    unsafe { Arc::increment_strong_count(context.cast::<Mem>()) };
}

unsafe extern "C" fn release_mem(context: *const c_void) {
    // SAFETY: each release gives back one reference `retain_mem` took.
    unsafe { Arc::decrement_strong_count(context.cast::<Mem>()) };
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
