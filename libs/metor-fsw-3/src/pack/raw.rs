//! The `repr(C)` types the ABI uses to pass byte slices and rings.
//!
//! Each one is a pointer and a length, with no lifetime. The receiver turns it
//! back into a Rust value through an `unsafe` accessor, and the caller is
//! responsible for upholding that accessor's contract.

use core::ffi::c_void;

use metor_fsw_3_ring::{RawOwner, RingBuffer, RingExport};

use crate::coordinator::{MakeError, ParamError};
use crate::io::Arena;

/// A borrowed array passed across the ABI.
///
/// It carries byte strings such as a type name, params, or an error. It also
/// carries the ring arrays `create` takes: one `RawSlice` of `RawRing`s per
/// input port, and one `RawSlice` of output `RawRing`s. `len` counts elements,
/// not bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RawSlice {
    pub ptr: *const c_void,
    pub len: usize,
}

impl RawSlice {
    /// An empty array. Exports return it when they have nothing to report.
    pub const EMPTY: RawSlice = RawSlice {
        ptr: core::ptr::null(),
        len: 0,
    };

    /// Creates a `RawSlice` pointing at `items`.
    ///
    /// `items` must outlive every read of the result.
    pub fn of<T>(items: &[T]) -> RawSlice {
        RawSlice {
            ptr: items.as_ptr().cast(),
            len: items.len(),
        }
    }

    /// Reads this array back as a slice of `T`.
    ///
    /// The caller picks the returned lifetime, as with any raw pointer.
    ///
    /// # Safety
    /// Either `len` is zero, or `ptr` is aligned for `T` and points at `len`
    /// initialized `T`s. Those values must stay alive and unmodified for as long
    /// as the returned slice is used.
    pub unsafe fn as_slice<'a, T>(&self) -> &'a [T] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: the caller's contract is exactly `from_raw_parts`'.
        unsafe { core::slice::from_raw_parts(self.ptr.cast(), self.len) }
    }

    /// Reads this array back as bytes.
    ///
    /// # Safety
    /// The same as for [`as_slice`](RawSlice::as_slice), with `T` as `u8`.
    pub unsafe fn as_bytes<'a>(&self) -> &'a [u8] {
        unsafe { self.as_slice() }
    }
}

/// A ring's memory region passed across the ABI.
///
/// `base` and `len` are the region [`RingBuffer::region`](metor_fsw_3_ring::RingBuffer::region)
/// reports. `owner` holds callbacks that keep the region alive while the
/// receiver uses it. A ring with no owner lives in the receiver's own memory,
/// which outlives every handle the receiver attaches to it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawRing {
    pub base: *mut u8,
    pub len: usize,
    pub owner: Option<RawOwner>,
}

impl RawRing {
    /// No region.
    pub const EMPTY: Self = Self {
        base: core::ptr::null_mut(),
        len: 0,
        owner: None,
    };

    /// Creates a `RawRing` from `export`.
    ///
    /// `export` must stay alive until the receiver attaches to the ring.
    pub fn of(export: &RingExport) -> Self {
        let (base, len) = export.region();
        Self {
            base,
            len,
            owner: Some(export.owner()),
        }
    }

    /// The region of `arena`, with an owner that keeps it alive. Empty before
    /// the arena is set.
    pub(crate) fn from_arena(arena: &Arena) -> Self {
        arena.region().map_or(Self::EMPTY, |(base, len)| Self {
            base: base.as_ptr(),
            len,
            owner: arena.owner(),
        })
    }

    /// Attaches to the ring in this region.
    ///
    /// # Safety
    /// The region holds a ring. An owner's callbacks are live and
    /// thread-safe. A region with no owner outlives every handle to the ring.
    pub(crate) unsafe fn attach(&self) -> Result<RingBuffer, MakeError> {
        let attached = match self.owner {
            // SAFETY: the caller's contract.
            Some(owner) => unsafe { RingBuffer::attach_owned(self.base, self.len, owner) },
            // SAFETY: the caller's contract.
            None => unsafe { RingBuffer::attach_raw(self.base, self.len) },
        };
        attached.map_err(|e| ParamError::Decode(e.to_string()).into())
    }
}

/// The size of a [`RawSlice`] in a wasm32 guest.
pub const WASM32_RAW_SLICE: usize = 8;
/// The size of a [`RawRing`] in a wasm32 guest.
pub const WASM32_RAW_RING: usize = 20;

// `RawOwner`'s function pointers are never null, so `Option` adds no size.
const _: () = assert!(size_of::<Option<RawOwner>>() == size_of::<RawOwner>());
#[cfg(target_arch = "wasm32")]
const _: () =
    assert!(size_of::<RawSlice>() == WASM32_RAW_SLICE && size_of::<RawRing>() == WASM32_RAW_RING);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_slice_roundtrip() {
        let bytes = b"metor".to_vec();
        let raw = RawSlice::of(&bytes);
        assert_eq!(raw.len, 5);
        // SAFETY: `bytes` outlives the borrow.
        assert_eq!(unsafe { raw.as_bytes() }, b"metor");

        let numbers = [7u64];
        let raw = RawSlice::of(&numbers);
        // SAFETY: `numbers` outlives the borrow.
        assert_eq!(unsafe { raw.as_slice::<u64>() }, &numbers);
    }

    #[test]
    fn test_empty_slices() {
        for raw in [RawSlice::EMPTY, RawSlice::of::<u8>(&[])] {
            assert_eq!(raw.len, 0);
            // SAFETY: a zero length never reads the pointer.
            assert!(unsafe { raw.as_bytes() }.is_empty());
        }
    }
}
