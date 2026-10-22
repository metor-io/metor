//! An arena is one system's fixed IO memory; a remake reuses it from the
//! start.

use core::cell::{Cell, OnceCell};
use core::ffi::c_void;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use metor_fsw_3_ring::{Lease, RawOwner};

use super::{Op, Span};
use crate::coordinator::MakeError;
use crate::pack::raw::RawRing;

#[derive(Default)]
pub(crate) struct Arena {
    mem: OnceCell<Mem>,
    epoch: Cell<u32>,
}

struct Mem {
    base: NonNull<u8>,
    len: usize,
    keep: Keep,
}

/// What keeps an arena's memory alive.
pub(crate) enum Keep {
    Owned(Arc<Owned>),
    Lease(Lease),
    /// Memory that outlives the arena, such as a wasm guest's.
    Unowned,
}

impl Arena {
    /// An arena whose memory is not set yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Sets the memory to `len` bytes at `base`. False if it was already set.
    ///
    /// # Safety
    /// The bytes stay alive and unmoved while `keep` lives, or for the
    /// arena's whole life when `keep` is `Unowned`.
    pub(crate) unsafe fn set(&self, base: NonNull<u8>, len: usize, keep: Keep) -> bool {
        self.mem.set(Mem { base, len, keep }).is_ok()
    }

    /// Sets the memory to the region `ring` names, unless it is already set
    /// or the region is empty.
    ///
    /// # Safety
    /// A non-empty `ring` is live, and its owner meets `RawOwner`'s contract.
    /// A region with no owner outlives the arena, such as memory a wasm host
    /// wrote into its guest.
    pub(crate) unsafe fn adopt(&self, ring: RawRing) {
        let base = NonNull::new(ring.base).filter(|_| ring.len > 0);
        let Some(base) = base.filter(|_| self.region().is_none()) else {
            return;
        };
        let keep = match ring.owner {
            // SAFETY: the caller's contract.
            Some(owner) => Keep::Lease(unsafe { Lease::acquire(owner) }),
            None => Keep::Unowned,
        };
        // SAFETY: the caller's contract, and `keep` holds the owner's reference.
        unsafe { self.set(base, ring.len, keep) };
    }

    /// Sets the memory to `bytes`. The memory is set once.
    pub(crate) fn set_owned(&self, bytes: Box<[u8]>) {
        let owned = Arc::new(Owned(NonNull::from(Box::leak(bytes))));
        let (base, len) = (owned.0.cast(), owned.0.len());
        // SAFETY: `owned` holds the bytes and lives in the arena.
        let set = unsafe { self.set(base, len, Keep::Owned(owned)) };
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

    /// An owner that keeps the memory alive, to share it across the pack
    /// ABI. A lease passes on its own owner.
    pub(crate) fn owner(&self) -> Option<RawOwner> {
        match &self.mem.get()?.keep {
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
/// It derefs to its filled bytes, `start..start + len` of the span. An op
/// takes it by value and hands it back with the result.
pub struct Buf {
    arena: Rc<Arena>,
    span: Span,
    start: u32,
    len: u32,
    epoch: u32,
}

impl Buf {
    fn new(arena: &Rc<Arena>, span: Span, len: u32) -> Self {
        debug_assert!(len <= span.len(), "filled past the span");
        Self {
            arena: arena.clone(),
            span,
            start: 0,
            len,
            epoch: arena.epoch(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.span.len() as usize
    }

    /// The bytes free past the filled ones.
    pub fn spare(&self) -> usize {
        self.capacity() - self.end()
    }

    /// Copies as much of `bytes` as fits and makes them the filled bytes.
    pub fn fill(&mut self, bytes: &[u8]) -> usize {
        self.clear();
        self.extend(bytes)
    }

    /// Copies as much of `bytes` as fits after the filled bytes, and returns
    /// how many it took.
    pub fn extend(&mut self, bytes: &[u8]) -> usize {
        let Some(whole) = self.whole() else {
            return 0;
        };
        let (at, n) = (self.end(), bytes.len().min(self.spare()));
        // SAFETY: `at..at + n` lies in this `Buf`'s span, which nothing else
        // reaches while `&mut self` is held.
        unsafe { core::slice::from_raw_parts_mut(whole.cast::<u8>().as_ptr().add(at), n) }
            .copy_from_slice(&bytes[..n]);
        self.len += n as u32;
        n
    }

    /// Drops the first `n` filled bytes, or all of them if there are fewer.
    pub fn advance(&mut self, n: usize) {
        let n = n.min(self.len as usize) as u32;
        self.start += n;
        self.len -= n;
    }

    pub fn clear(&mut self) {
        self.start = 0;
        self.len = 0;
    }

    /// Sets the filled length, clamped to the bytes past the start.
    pub fn set_len(&mut self, n: usize) {
        self.len = n.min(self.capacity() - self.start as usize) as u32;
    }

    /// Makes the first `n` bytes of the span the filled ones, as a fill op
    /// leaves them.
    pub(crate) fn set_filled(&mut self, n: usize) {
        self.start = 0;
        self.set_len(n);
    }

    /// Splits the span at `at`, returning the empty tail. `None` if `at` is
    /// past the capacity or inside the filled bytes.
    pub fn split_off(&mut self, at: usize) -> Option<Buf> {
        let at = u32::try_from(at).ok()?;
        if at < self.end() as u32 || at > self.span.len() {
            return None;
        }
        let tail = Span::new(self.span.off() + at, self.span.len() - at);
        self.span = Span::new(self.span.off(), at);
        Some(Self {
            arena: self.arena.clone(),
            span: tail,
            start: 0,
            len: 0,
            epoch: self.epoch,
        })
    }

    /// Rejoins `tail`, a span that starts where this one ends, from the same
    /// arena and make. Its filled bytes join this one's only when they meet
    /// at the seam. Hands `tail` back if it cannot join.
    pub fn join(&mut self, tail: Buf) -> Result<(), Buf> {
        let adjacent = self.span.end() == Some(tail.span.off());
        let same = Rc::ptr_eq(&self.arena, &tail.arena) && self.epoch == tail.epoch;
        let meets = tail.len == 0 || (self.spare() == 0 && tail.start == 0);
        if !(adjacent && same && meets) {
            return Err(tail);
        }
        self.span = Span::new(self.span.off(), self.span.len() + tail.span.len());
        self.len += tail.len;
        Ok(())
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
            false => Span::new(self.span.off() + self.start, self.len),
        }
    }

    /// The end of the filled bytes, from the span's start.
    fn end(&self) -> usize {
        (self.start + self.len) as usize
    }

    fn whole(&self) -> Option<NonNull<[u8]>> {
        (self.arena.epoch() == self.epoch)
            .then(|| self.arena.get_slice(self.span))
            .flatten()
    }

    /// The filled bytes inside `whole`.
    fn filled(&self, whole: NonNull<[u8]>) -> *mut u8 {
        // SAFETY: `start` never passes the span's length.
        unsafe { whole.cast::<u8>().as_ptr().add(self.start as usize) }
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
                core::slice::from_raw_parts(self.filled(whole), self.len as usize)
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
                core::slice::from_raw_parts_mut(self.filled(whole), self.len as usize)
            },
            None => &mut [],
        }
    }
}

impl core::fmt::Debug for Buf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Buf")
            .field("span", &self.span)
            .field("start", &self.start)
            .field("len", &self.len)
            .finish()
    }
}

/// Allocates a system's buffers while its constructor runs.
pub struct IoInit<'a> {
    arena: Option<&'a Rc<Arena>>,
    /// The system the buffers are for, named in an `IoMemory` error.
    system: &'a str,
    /// The first make's bytes, before the arena's memory is set.
    staged: Vec<u8>,
    top: u32,
}

impl<'a> IoInit<'a> {
    /// Starts system `system`'s make on `arena`, from its first byte.
    pub(crate) fn new(arena: &'a Rc<Arena>, system: &'a str) -> Self {
        arena.epoch.set(arena.epoch().wrapping_add(1));
        Self {
            arena: Some(arena),
            system,
            staged: Vec::new(),
            top: 0,
        }
    }

    /// An `IoInit` for system `system`, which has no arena. Every allocation
    /// fails with `IoMemory`.
    pub(crate) fn empty(system: &'a str) -> Self {
        Self {
            arena: None,
            system,
            staged: Vec::new(),
            top: 0,
        }
    }

    /// Allocates `len` zeroed bytes.
    ///
    /// # Errors
    /// Fails with `IoMemory` if the arena cannot hold them.
    pub fn alloc(&mut self, len: usize) -> Result<Buf, MakeError> {
        self.alloc_with(len, &[])
    }

    /// Allocates a buffer filled with `bytes`.
    ///
    /// # Errors
    /// Fails with `IoMemory` if the arena cannot hold them.
    pub fn alloc_from(&mut self, bytes: &[u8]) -> Result<Buf, MakeError> {
        self.alloc_with(bytes.len(), bytes)
    }

    /// Allocates `len` bytes starting with `fill`, zeroing the rest.
    fn alloc_with(&mut self, len: usize, fill: &[u8]) -> Result<Buf, MakeError> {
        let (arena, span) = self
            .arena
            .zip(self.bump(len))
            .ok_or_else(|| self.out_of_memory(len))?;
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
        Ok(Buf::new(arena, span, fill.len() as u32))
    }

    /// The error for an allocation of `len` bytes that does not fit.
    fn out_of_memory(&self, len: usize) -> MakeError {
        MakeError::IoMemory {
            system: self.system.to_string(),
            requested_len: len,
            available: self.arena.and_then(|a| a.region()).map(|(_, len)| len),
        }
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

    /// Ends the make and gives a first make's arena its memory.
    pub(crate) fn finish(self) {
        if let Some(arena) = self.arena.filter(|a| a.region().is_none()) {
            arena.set_owned(self.staged.into_boxed_slice());
        }
    }
}

#[cfg(kani)]
impl Buf {
    /// A buffer spanning its own arena of `bytes`, none filled. Built
    /// directly: a make sizes its arena by rounding up, which Kani would
    /// unwind byte by byte.
    pub(crate) fn owned(bytes: Box<[u8]>) -> Self {
        let arena = Rc::new(Arena::new());
        let len = bytes.len() as u32;
        arena.set_owned(bytes);
        Self::new(&arena, Span::new(0, len), 0)
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

    fn eight() -> Buf {
        Buf::owned(Box::new([0u8; 8]))
    }

    /// Whether `inner` lies inside `outer`.
    fn within(inner: Span, outer: Span) -> bool {
        inner.off() >= outer.off() && inner.end() <= outer.end()
    }

    /// After any sequence of fills, extends, advances, and length sets, an
    /// op on the buffer names only bytes of its span.
    #[kani::proof]
    #[kani::unwind(5)]
    fn ops_stay_in_the_span() {
        let mut buf = eight();
        let span = buf.span;
        for _ in 0..3 {
            let n: usize = kani::any();
            kani::assume(n <= 9);
            match kani::any::<u8>() % 4 {
                0 => drop(buf.fill(&[0; 9][..n])),
                1 => drop(buf.extend(&[0; 9][..n])),
                2 => buf.advance(n),
                _ => buf.set_len(n),
            }
            assert!(buf.start as usize + buf.len as usize <= buf.capacity());
            assert!(within(buf.span_for(&Op::Send), span));
            assert!(within(buf.span_for(&Op::Recv), span));
        }
    }

    /// A full head joins a tail with filled bytes, keeping the head's then
    /// the tail's.
    #[kani::proof]
    fn join_keeps_a_filled_tail() {
        let mut buf = eight();
        let span = buf.span;
        let (at, filled): (usize, usize) = (kani::any(), kani::any());
        kani::assume(at <= 8);
        buf.set_len(at);
        let mut tail = buf.split_off(at).expect("the head is full");
        tail.set_len(filled);
        let tail_len = tail.len as usize;
        assert!(buf.join(tail).is_ok());
        assert_eq!(buf.span, span);
        assert_eq!(buf.len as usize, at + tail_len);
        assert!(within(buf.span_for(&Op::Send), span));
    }

    /// A split, then a join of its tail, gives back the original span.
    #[kani::proof]
    fn split_then_join_restores() {
        let mut buf = eight();
        let span = buf.span;
        let (filled, at): (usize, usize) = (kani::any(), kani::any());
        kani::assume(filled <= 8);
        buf.set_len(filled);
        if let Some(tail) = buf.split_off(at) {
            assert_eq!(buf.span.end(), Some(tail.span.off()));
            assert!(buf.join(tail).is_ok());
            assert_eq!(buf.span, span);
            assert_eq!(buf.len as usize, filled);
        } else {
            assert!(at < filled || at > 8);
        }
    }
}
