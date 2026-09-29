//! Storage for ring regions: heap, memory-mapped, or owned by the caller.

use std::cell::UnsafeCell;

use crate::owner::{Lease, RawOwner};

/// A 16-byte aligned unit of storage that allows interior mutation.
#[repr(C, align(16))]
pub(super) struct Word(pub(super) UnsafeCell<[u64; 2]>);

/// A byte region, owned or borrowed, whose base address never moves.
pub(super) struct Backing {
    pub(super) base: *mut u8,
    pub(super) len: usize,
    _owner: BackingOwner,
}

enum BackingOwner {
    /// Held as a raw pointer rather than a `Box`, so pointers derived from the
    /// allocation stay valid when the `Backing` moves.
    Heap {
        words: *mut [Word],
    },
    #[cfg(feature = "mmap")]
    Mmap {
        _map: memmap2::MmapMut,
    },
    Raw,
    Owned {
        _lease: Lease,
    },
}

impl Backing {
    /// Allocates zeroed, 16-byte aligned storage.
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

    /// Borrows storage without taking ownership.
    ///
    /// # Safety
    /// The storage is one allocation that allows interior mutation. It must stay
    /// live for every handle and borrow that uses it, and its owner must allow
    /// access from other threads.
    pub(super) unsafe fn raw(base: *mut u8, len: usize) -> Self {
        Self {
            base,
            len,
            _owner: BackingOwner::Raw,
        }
    }

    /// # Safety
    /// `owner` meets `RawOwner`'s contract and keeps the entire region alive.
    pub(super) unsafe fn owned(base: *mut u8, len: usize, owner: RawOwner) -> Self {
        // SAFETY: the caller guarantees a live, thread-safe owner.
        let lease = unsafe { Lease::acquire(owner) };
        Self {
            base,
            len,
            _owner: BackingOwner::Owned { _lease: lease },
        }
    }

    /// Takes ownership of a shared mapping.
    ///
    /// The mapping lives until the last ring handle is dropped.
    #[cfg(feature = "mmap")]
    pub(super) fn mmap(map: memmap2::MmapMut) -> Self {
        let (base, len) = (map.as_ptr() as *mut u8, map.len());
        Self {
            base,
            len,
            _owner: BackingOwner::Mmap { _map: map },
        }
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
