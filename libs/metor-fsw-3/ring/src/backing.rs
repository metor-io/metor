//! Storage ownership for heap, mapped, and caller-owned regions.

use std::cell::UnsafeCell;

/// A 16-byte aligned, interior-mutable storage unit.
#[repr(C, align(16))]
pub(super) struct Word(pub(super) UnsafeCell<[u64; 2]>);

/// An owned or borrowed byte region with a stable base address.
pub(super) struct Backing {
    base: *mut u8,
    len: usize,
    _owner: BackingOwner,
}

enum BackingOwner {
    /// Raw ownership preserves pointers derived from the allocation across moves.
    Heap {
        words: *mut [Word],
    },
    #[cfg(feature = "mmap")]
    Mmap {
        _map: memmap2::MmapMut,
    },
    Raw,
}

impl Backing {
    /// Allocate zeroed, 16-byte aligned storage.
    pub(super) fn heap(size: usize) -> Self {
        let count = size.div_ceil(size_of::<Word>());
        let buf: Box<[Word]> = (0..count)
            .map(|_| Word(UnsafeCell::new([0u64; 2])))
            .collect();
        let len = buf.len() * size_of::<Word>();
        // Derive pointers only after transferring allocation ownership.
        let words = Box::into_raw(buf);
        Self {
            base: words.cast::<u8>(),
            len,
            _owner: BackingOwner::Heap { words },
        }
    }

    /// Borrow storage without taking ownership.
    ///
    /// # Safety
    /// The single, interior-mutable allocation must remain live for every handle
    /// and borrow using it. Its owner must allow access from other threads.
    pub(super) unsafe fn raw(base: *mut u8, len: usize) -> Self {
        Self {
            base,
            len,
            _owner: BackingOwner::Raw,
        }
    }

    /// Own a shared mapping until the last ring handle is dropped.
    #[cfg(feature = "mmap")]
    pub(super) fn mmap(map: memmap2::MmapMut) -> Self {
        let (base, len) = (map.as_ptr() as *mut u8, map.len());
        Self {
            base,
            len,
            _owner: BackingOwner::Mmap { _map: map },
        }
    }

    #[inline]
    pub(super) fn base(&self) -> *mut u8 {
        self.base
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        if let BackingOwner::Heap { words } = self._owner {
            // SAFETY: reconstruct the sole owning pointer exactly once.
            drop(unsafe { Box::from_raw(words) });
        }
    }
}

// SAFETY: callers uphold storage lifetimes; the ring synchronizes all shared access.
// Backing owners may be dropped on any thread.
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}
