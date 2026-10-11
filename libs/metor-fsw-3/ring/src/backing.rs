//! Storage for ring regions: heap, memory-mapped, or owned by the caller.

use std::alloc::{self, Layout};
use std::ptr::NonNull;

use crate::PAYLOAD_ALIGNMENT;
use crate::owner::{Lease, RawOwner};

/// A byte region, owned or borrowed, whose base address never moves.
pub(super) struct Backing {
    pub(super) base: *mut u8,
    pub(super) len: usize,
    _owner: BackingOwner,
}

enum BackingOwner {
    Heap {
        _region: HeapRegion,
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

/// A zeroed, 16-byte aligned heap allocation that is freed on drop.
///
/// It holds a raw pointer rather than a `Box`, so pointers derived from the
/// allocation stay valid when it moves.
struct HeapRegion {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl HeapRegion {
    /// Allocates at least `size` zeroed bytes, rounded up to 16.
    fn new(size: usize) -> Self {
        // PANIC Safety: ring geometry keeps sizes at or below `isize::MAX` minus the alignment.
        let layout = Layout::from_size_align(size.max(1), PAYLOAD_ALIGNMENT)
            .expect("heap ring size fits a Layout")
            .pad_to_align();
        // SAFETY: the layout has a nonzero size.
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        let Some(ptr) = NonNull::new(ptr) else {
            alloc::handle_alloc_error(layout)
        };
        Self { ptr, layout }
    }
}

impl Drop for HeapRegion {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc_zeroed` with this layout and is freed once.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

impl Backing {
    /// Allocates zeroed, 16-byte aligned storage.
    pub(super) fn heap(size: usize) -> Self {
        let region = HeapRegion::new(size);
        Self {
            base: region.ptr.as_ptr(),
            len: region.layout.size(),
            _owner: BackingOwner::Heap { _region: region },
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

    /// Borrows storage that `owner` keeps alive, holding one reference on it.
    ///
    /// # Safety
    /// `owner` must meet [`RawOwner`]'s contract, and the reference it holds must
    /// keep all `len` bytes at `base` alive.
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

// SAFETY: callers uphold storage lifetimes; the ring synchronizes all shared access.
// Backing owners may be dropped on any thread.
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}
