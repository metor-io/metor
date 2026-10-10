//! `Arena`, one system's IO memory; `Buf`, a span of it; and `IoInit`, which
//! allocates spans while the system is made.
//!
//! An arena is a bump allocator. It grows while its system's first make runs,
//! then freezes, and its address never changes again, so the kernel and other
//! parties may hold pointers into it. A remake rewinds it to the start and
//! must fit. Each rewind starts a new epoch, and a `Buf` from an older epoch
//! reads as empty.

use core::cell::{Cell, UnsafeCell};
use core::ffi::c_void;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use metor_fsw_3_ring::{Lease, RawOwner};

use super::{Op, Span};
use crate::coordinator::ParamError;

pub(crate) struct Arena {
    mem: UnsafeCell<Mem>,
    top: Cell<u32>,
    epoch: Cell<u32>,
}

enum Mem {
    Growing(Vec<u8>),
    Fixed {
        base: NonNull<u8>,
        len: usize,
        _keep: Keep,
    },
}

/// What keeps a frozen arena's memory alive.
pub(crate) enum Keep {
    Owned(Arc<Owned>),
    Lease(Lease),
    /// Memory that outlives the arena, such as a wasm guest's.
    Unowned,
}

impl Arena {
    pub(crate) fn new() -> Self {
        Self::with(Mem::Growing(Vec::new()))
    }

    /// A frozen arena over `len` bytes at `base`.
    ///
    /// # Safety
    /// The bytes stay alive and unmoved while `keep` lives, or for the
    /// arena's whole life when `keep` is `Unowned`.
    pub(crate) unsafe fn over(base: NonNull<u8>, len: usize, keep: Keep) -> Self {
        Self::with(Mem::Fixed {
            base,
            len,
            _keep: keep,
        })
    }

    /// A frozen arena that owns `bytes`.
    pub(crate) fn owning(bytes: Box<[u8]>) -> Self {
        let owned = Arc::new(Owned(NonNull::from(Box::leak(bytes))));
        let (base, len) = (owned.0.cast(), owned.0.len());
        // SAFETY: `owned` holds the bytes and lives in the arena.
        unsafe { Self::over(base, len, Keep::Owned(owned)) }
    }

    fn with(mem: Mem) -> Self {
        Self {
            mem: UnsafeCell::new(mem),
            top: Cell::new(0),
            epoch: Cell::new(0),
        }
    }

    fn mem(&self) -> &Mem {
        // SAFETY: `mem_mut` borrows end within the methods that take them.
        unsafe { &*self.mem.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn mem_mut(&self) -> &mut Mem {
        // SAFETY: the arena is not shared across threads, and no method
        // holds a borrow of `mem` while calling another.
        unsafe { &mut *self.mem.get() }
    }

    /// The frozen memory, or `None` while the first make may still grow it.
    pub(crate) fn region(&self) -> Option<(NonNull<u8>, usize)> {
        match self.mem() {
            Mem::Growing(_) => None,
            Mem::Fixed { base, len, .. } => Some((*base, *len)),
        }
    }

    pub(crate) fn frozen(&self) -> bool {
        self.region().is_some()
    }

    pub(crate) fn epoch(&self) -> u32 {
        self.epoch.get()
    }

    /// Allocates `cap` zeroed bytes.
    fn alloc(&self, cap: u32) -> Option<Span> {
        let off = self.top.get();
        let end = off.checked_add(cap)?;
        match self.mem_mut() {
            Mem::Growing(v) => v.resize(end as usize, 0),
            Mem::Fixed { base, len, .. } => {
                if end as usize > *len {
                    return None;
                }
                // SAFETY: `off..end` lies in the arena, past every live span.
                unsafe { base.add(off as usize).write_bytes(0, cap as usize) };
            }
        }
        self.top.set(end);
        Some(Span::new(off, cap))
    }

    /// Copies `bytes` to the start of a span just allocated.
    fn write(&self, span: Span, bytes: &[u8]) {
        let base = match self.mem_mut() {
            Mem::Growing(v) => v.as_mut_ptr(),
            Mem::Fixed { base, .. } => base.as_ptr(),
        };
        // SAFETY: `alloc` made room for `span`, and `bytes` fits in it.
        unsafe {
            let dst = base.add(span.off() as usize);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
    }

    /// Fixes the arena's size and address. A frozen arena stays as it is.
    fn freeze(&self) {
        if let Mem::Growing(v) = self.mem_mut() {
            let bytes = core::mem::take(v).into_boxed_slice();
            *self.mem_mut() = Self::owning(bytes).mem.into_inner();
        }
    }

    /// Starts the next make at the arena's first byte.
    fn rewind(&self) {
        self.top.set(0);
        self.epoch.set(self.epoch.get().wrapping_add(1));
    }

    /// The memory `span` names, if the arena is frozen and holds all of it.
    pub(crate) fn span(&self, span: Span) -> Option<NonNull<[u8]>> {
        let (base, len) = self.region()?;
        let end = span.end()? as usize;
        if end > len {
            return None;
        }
        // SAFETY: `span` lies inside the arena.
        let start = unsafe { base.add(span.off() as usize) };
        Some(NonNull::slice_from_raw_parts(start, span.len() as usize))
    }

    /// An owner that keeps the frozen memory alive, to share it across the
    /// pack ABI. A lease passes on its own owner.
    pub(crate) fn owner(&self) -> Option<RawOwner> {
        let Mem::Fixed { _keep: keep, .. } = self.mem() else {
            return None;
        };
        match keep {
            Keep::Owned(owned) => Some(RawOwner {
                context: Arc::as_ptr(owned).cast(),
                retain: retain_owned,
                release: release_owned,
            }),
            Keep::Lease(lease) => Some(lease.owner()),
            Keep::Unowned => None,
        }
    }
}

/// Memory leaked from a `Box` and freed by the last `Arc`.
pub(crate) struct Owned(NonNull<[u8]>);

// SAFETY: `Owned` only frees its memory. The bytes are reached through
// arenas, where each span belongs to one party at a time.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `Box::leak`, and is freed once.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

unsafe extern "C" fn retain_owned(context: *const c_void) {
    // SAFETY: `owner` made `context` from a live `Arc<Owned>`.
    unsafe { Arc::increment_strong_count(context.cast::<Owned>()) };
}

unsafe extern "C" fn release_owned(context: *const c_void) {
    // SAFETY: each release gives back one reference `retain_owned` took.
    unsafe { Arc::decrement_strong_count(context.cast::<Owned>()) };
}

/// A span of its system's arena.
///
/// It derefs to its filled bytes. An op takes it by value and hands it back
/// with the result.
pub struct Buf {
    arena: Option<Rc<Arena>>,
    span: Span,
    len: u32,
    epoch: u32,
}

impl Buf {
    /// A buffer of no capacity, in no arena.
    pub(crate) fn empty() -> Self {
        Self {
            arena: None,
            span: Span::default(),
            len: 0,
            epoch: 0,
        }
    }

    fn new(arena: &Rc<Arena>, span: Span, len: u32) -> Self {
        Self {
            arena: Some(arena.clone()),
            span,
            len,
            epoch: arena.epoch(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.span.len() as usize
    }

    /// Copies as much of `bytes` as fits and sets the length to it.
    pub fn fill(&mut self, bytes: &[u8]) -> usize {
        let Some(whole) = self.whole() else {
            return 0;
        };
        let n = bytes.len().min(self.capacity());
        // SAFETY: `whole` is this `Buf`'s span, which nothing else reaches
        // while `&mut self` is held.
        unsafe { core::slice::from_raw_parts_mut(whole.cast::<u8>().as_ptr(), n) }
            .copy_from_slice(&bytes[..n]);
        self.len = n as u32;
        n
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Sets the filled length, clamped to the capacity.
    pub fn set_len(&mut self, n: usize) {
        self.len = n.min(self.capacity()) as u32;
    }

    /// Whether the buffer is in a frozen arena and from its current make.
    pub(crate) fn live(&self) -> bool {
        self.whole().is_some()
    }

    /// The span an op on this buffer names: the whole capacity for an op
    /// that fills it, the filled bytes otherwise.
    pub(crate) fn span_for(&self, op: &Op) -> Span {
        match op.fills() {
            true => self.span,
            false => Span::new(self.span.off(), self.len),
        }
    }

    fn whole(&self) -> Option<NonNull<[u8]>> {
        let arena = self.arena.as_ref()?;
        (arena.epoch() == self.epoch)
            .then(|| arena.span(self.span))
            .flatten()
    }
}

impl Deref for Buf {
    type Target = [u8];

    /// The filled bytes. Empty while the arena is made, or after a remake.
    fn deref(&self) -> &[u8] {
        match self.whole() {
            // SAFETY: spans never overlap, an op in flight holds its `Buf`,
            // and a frozen arena never moves.
            Some(whole) => unsafe {
                core::slice::from_raw_parts(whole.cast().as_ptr(), self.len as usize)
            },
            None => &[],
        }
    }
}

impl DerefMut for Buf {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self.whole() {
            // SAFETY: as in `deref`, and `&mut self` makes the borrow unique.
            Some(whole) => unsafe {
                core::slice::from_raw_parts_mut(whole.cast().as_ptr(), self.len as usize)
            },
            None => &mut [],
        }
    }
}

impl core::fmt::Debug for Buf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Buf")
            .field("span", &self.span)
            .field("len", &self.len)
            .finish()
    }
}

/// Allocates a system's buffers while its constructor runs.
pub struct IoInit<'a> {
    arena: Option<&'a Rc<Arena>>,
    /// Bytes asked for and not given.
    short: Cell<usize>,
}

impl<'a> IoInit<'a> {
    /// Starts a make on `arena`, from its first byte.
    pub(crate) fn new(arena: &'a Rc<Arena>) -> Self {
        arena.rewind();
        Self {
            arena: Some(arena),
            short: Cell::new(0),
        }
    }

    /// An `IoInit` for a system with no arena. Every allocation fails the
    /// make.
    pub(crate) fn empty() -> Self {
        Self {
            arena: None,
            short: Cell::new(0),
        }
    }

    /// Allocates `len` zeroed bytes. If the arena cannot, returns an empty
    /// buffer and the make fails afterward.
    pub fn alloc(&self, len: usize) -> Buf {
        self.take(len)
            .map_or_else(Buf::empty, |(arena, span)| Buf::new(arena, span, 0))
    }

    /// Allocates a buffer filled with `bytes`.
    pub fn alloc_from(&self, bytes: &[u8]) -> Buf {
        let Some((arena, span)) = self.take(bytes.len()) else {
            return Buf::empty();
        };
        arena.write(span, bytes);
        Buf::new(arena, span, span.len())
    }

    fn take(&self, len: usize) -> Option<(&'a Rc<Arena>, Span)> {
        let given = self.arena.and_then(|arena| {
            let span = arena.alloc(u32::try_from(len).ok()?)?;
            Some((arena, span))
        });
        if given.is_none() {
            self.short.set(self.short.get().saturating_add(len));
        }
        given
    }

    /// Fails with `IoMemory` if any allocation was refused.
    pub(crate) fn finish(self) -> Result<(), ParamError> {
        let need = self.short.get();
        if need == 0 {
            return Ok(());
        }
        let reason = match self.arena.and_then(|a| a.region()) {
            Some((_, len)) => format!("needs {need} bytes more than the arena's {len}"),
            None => format!("needs {need} bytes, but the system has no `Cap`"),
        };
        Err(ParamError::IoMemory { need, reason })
    }
}

impl Drop for IoInit<'_> {
    fn drop(&mut self) {
        if let Some(arena) = self.arena {
            arena.freeze();
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// For any span, `span` returns memory inside the arena or nothing.
    #[kani::proof]
    fn span_stays_in_the_arena() {
        let arena = Arena::owning(Box::new([0u8; 8]));
        let (base, len) = arena.region().expect("frozen");
        let span = Span::new(kani::any(), kani::any());
        if let Some(got) = arena.span(span) {
            let start = got.cast::<u8>().as_ptr() as usize - base.as_ptr() as usize;
            assert!(start + got.len() <= len);
        }
    }
}
