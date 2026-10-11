//! The `repr(C)` views the ABI passes byte slices and ring regions through.
//!
//! Each is a pointer and an element count, with no lifetime. The reader
//! restores one through an `unsafe` accessor whose contract the caller keeps.

use core::ffi::c_void;

/// A borrowed array: the descriptor, a type name, params, an error, or the
/// `RawPort` and `RawRing` arrays `create` takes. `len` counts elements.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RawSlice {
    pub ptr: *const c_void,
    pub len: usize,
}

impl RawSlice {
    /// The empty array, which is what an export returns when it has nothing.
    pub const EMPTY: RawSlice = RawSlice {
        ptr: core::ptr::null(),
        len: 0,
    };

    /// Points at `items`, which must outlive every read of the result.
    pub fn of<T>(items: &[T]) -> RawSlice {
        RawSlice {
            ptr: items.as_ptr().cast(),
            len: items.len(),
        }
    }

    /// Reads this array back as a slice of `T`. The returned lifetime is the
    /// caller's to choose, as it is for any raw pointer.
    ///
    /// # Safety
    /// Either `len` is zero, or `ptr` is `T`-aligned and points at `len`
    /// initialized `T`s that live, unwritten, for the returned slice's life.
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
    /// As [`as_slice`](RawSlice::as_slice), with `T` as `u8`.
    pub unsafe fn as_bytes<'a>(&self) -> &'a [u8] {
        unsafe { self.as_slice() }
    }
}

/// One ring's region, as [`RingBuffer::region`](metor_fsw_3_ring::RingBuffer::region) reports it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawRing {
    pub base: *mut u8,
    pub len: usize,
    pub owner: metor_fsw_3_ring::RawOwner,
}

impl RawRing {
    /// Borrows an export that must remain alive until the receiver attaches.
    pub fn of(export: &metor_fsw_3_ring::RingExport) -> Self {
        let (base, len) = export.region();
        Self {
            base,
            len,
            owner: export.owner(),
        }
    }
}

/// One input port: its producers' rings, in edge order.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RawPort {
    pub rings: *const RawRing,
    pub len: usize,
}

impl RawPort {
    /// The rings feeding this port.
    ///
    /// # Safety
    /// Either `len` is zero, or `rings` is `RawRing`-aligned and points at
    /// `len` initialized `RawRing`s that live for the returned slice's life.
    pub unsafe fn rings<'a>(&self) -> &'a [RawRing] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: the caller's contract is exactly `from_raw_parts`'.
        unsafe { core::slice::from_raw_parts(self.rings, self.len) }
    }
}

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
    fn test_empty_null_slices() {
        // SAFETY: a zero length never reads the pointer.
        assert!(unsafe { RawSlice::EMPTY.as_bytes() }.is_empty());
        let port = RawPort {
            rings: core::ptr::null(),
            len: 0,
        };
        // SAFETY: a zero length never reads the pointer.
        assert!(unsafe { port.rings() }.is_empty());
    }

    #[test]
    fn test_empty_raw_slice() {
        let raw = RawSlice::of::<u8>(&[]);
        assert_eq!(raw.len, 0);
        // SAFETY: a zero length never reads the pointer.
        assert!(unsafe { raw.as_bytes() }.is_empty());
    }
}
