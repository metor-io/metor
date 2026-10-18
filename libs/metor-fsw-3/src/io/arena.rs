//! An arena is one system's fixed IO memory; a remake reuses it from the
//! start.

use core::cell::{Cell, OnceCell};
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use super::{Op, Span};
use crate::coordinator::ParamError;

#[derive(Default)]
pub(crate) struct Arena {
    mem: OnceCell<Mem>,
    epoch: Cell<u32>,
}

struct Mem {
    base: NonNull<u8>,
    len: usize,
    _owned: Arc<Owned>,
}

impl Arena {
    /// An arena whose memory is not set yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Sets the memory to `bytes`. The memory is set once.
    pub(crate) fn set_owned(&self, bytes: Box<[u8]>) {
        let owned = Arc::new(Owned(NonNull::from(Box::leak(bytes))));
        let (base, len) = (owned.0.cast(), owned.0.len());
        let mem = Mem {
            base,
            len,
            _owned: owned,
        };
        let set = self.mem.set(mem).is_ok();
        debug_assert!(set, "an arena's memory is set once");
    }

    /// The memory, once set.
    pub(crate) fn region(&self) -> Option<(NonNull<u8>, usize)> {
        self.mem.get().map(|m| (m.base, m.len))
    }

    pub(crate) fn epoch(&self) -> u32 {
        self.epoch.get()
    }

    /// The memory `span` names, if the arena is set and holds all of it.
    pub(crate) fn get_slice(&self, span: Span) -> Option<NonNull<[u8]>> {
        let (base, len) = self.region()?;
        let end = span.end()? as usize;
        if end > len {
            return None;
        }
        // SAFETY: `span` lies inside the arena.
        let start = unsafe { base.add(span.off() as usize) };
        Some(NonNull::slice_from_raw_parts(start, span.len() as usize))
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

    /// Whether the buffer's arena is set and the buffer is from its current
    /// make.
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
            .then(|| arena.get_slice(self.span))
            .flatten()
    }
}

impl Deref for Buf {
    type Target = [u8];

    /// The filled bytes. Empty until the arena is set, and after a remake.
    fn deref(&self) -> &[u8] {
        match self.whole() {
            // SAFETY: spans never overlap, an op in flight holds its `Buf`,
            // and an arena's memory never moves.
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
    /// The first make's bytes, before the arena's memory is set.
    staged: Vec<u8>,
    top: u32,
    /// Bytes asked for and not given.
    short: usize,
}

impl<'a> IoInit<'a> {
    /// Starts a make on `arena`, from its first byte.
    pub(crate) fn new(arena: &'a Rc<Arena>) -> Self {
        arena.epoch.set(arena.epoch().wrapping_add(1));
        Self {
            arena: Some(arena),
            staged: Vec::new(),
            top: 0,
            short: 0,
        }
    }

    /// An `IoInit` for a system with no arena. Every allocation fails the
    /// make.
    pub(crate) fn empty() -> Self {
        Self {
            arena: None,
            staged: Vec::new(),
            top: 0,
            short: 0,
        }
    }

    /// Allocates `len` zeroed bytes. If the arena cannot, returns an empty
    /// buffer and the make fails afterward.
    pub fn alloc(&mut self, len: usize) -> Buf {
        self.alloc_with(len, &[])
    }

    /// Allocates a buffer filled with `bytes`.
    pub fn alloc_from(&mut self, bytes: &[u8]) -> Buf {
        self.alloc_with(bytes.len(), bytes)
    }

    /// Allocates `len` bytes starting with `fill`, zeroing the rest.
    fn alloc_with(&mut self, len: usize, fill: &[u8]) -> Buf {
        let span = self.arena.zip(self.bump(len));
        let Some((arena, span)) = span else {
            self.short = self.short.saturating_add(len);
            return Buf::empty();
        };
        let (off, end) = (span.off() as usize, span.off() as usize + len);
        match arena.region() {
            // SAFETY: `bump` kept `off..end` inside the memory and past every
            // span this make handed out.
            Some((base, _)) => unsafe {
                let dst = core::slice::from_raw_parts_mut(base.as_ptr().add(off), len);
                dst.fill(0);
                dst[..fill.len()].copy_from_slice(fill);
            },
            None => {
                self.staged.resize(end, 0);
                self.staged[off..off + fill.len()].copy_from_slice(fill);
            }
        }
        Buf::new(arena, span, fill.len() as u32)
    }

    /// The next `len` bytes, if they fit: past the end of a set arena's
    /// memory fails, and staged memory grows.
    fn bump(&mut self, len: usize) -> Option<Span> {
        let cap = u32::try_from(len).ok()?;
        let off = self.top;
        let end = off.checked_add(cap)?;
        let fits = self
            .arena?
            .region()
            .is_none_or(|(_, size)| end as usize <= size);
        fits.then(|| {
            self.top = end;
            Span::new(off, cap)
        })
    }

    /// Ends the make and gives a first make's arena its memory. Fails with
    /// `IoMemory`, naming system `system`, if any allocation was refused.
    pub(crate) fn finish(self, system: &str) -> Result<(), ParamError> {
        if let Some(arena) = self.arena.filter(|a| a.region().is_none()) {
            arena.set_owned(self.staged.into_boxed_slice());
        }
        let requested_len = self.short;
        if requested_len == 0 {
            return Ok(());
        }
        Err(ParamError::IoMemory {
            system: system.to_string(),
            requested_len,
            available: self.arena.and_then(|a| a.region()).map(|(_, len)| len),
        })
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// For any span, `get_slice` returns memory inside the arena or nothing.
    #[kani::proof]
    fn span_stays_in_the_arena() {
        let arena = Arena::new();
        arena.set_owned(Box::new([0u8; 8]));
        let (base, len) = arena.region().expect("set");
        let span = Span::new(kani::any(), kani::any());
        if let Some(got) = arena.get_slice(span) {
            let start = got.cast::<u8>().as_ptr() as usize - base.as_ptr() as usize;
            assert!(start + got.len() <= len);
        }
    }
}
