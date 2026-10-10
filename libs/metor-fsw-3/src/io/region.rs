//! Arena memory that another party owns, or that this side mirrors.

use core::ptr::NonNull;

use metor_fsw_3_ring::RawOwner;

/// A buffer table's memory when it is not the table's own allocation.
pub(crate) enum Region {
    /// Memory another party owns, such as a dylib's arena. The lease keeps
    /// it alive.
    Shared {
        base: NonNull<u8>,
        len: usize,
        _lease: Lease,
    },
    /// Memory this side owns, mirroring a wasm guest's arena.
    Mirror { bytes: Box<[u8]> },
}

impl Region {
    /// The region at `base..base + len`, kept alive by `owner`.
    ///
    /// # Safety
    /// `owner` meets `RawOwner`'s contract and keeps `len` bytes at `base`
    /// alive and unmoved until its last release.
    pub(crate) unsafe fn shared(base: NonNull<u8>, len: usize, owner: RawOwner) -> Self {
        // SAFETY: the caller's contract.
        let _lease = unsafe { Lease::acquire(owner) };
        Self::Shared { base, len, _lease }
    }

    pub(crate) fn mirror(bytes: Box<[u8]>) -> Self {
        Self::Mirror { bytes }
    }

    pub(crate) fn base(&mut self) -> NonNull<u8> {
        match self {
            Self::Shared { base, .. } => *base,
            Self::Mirror { bytes } => NonNull::from(&mut bytes[..]).cast(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Shared { len, .. } => *len,
            Self::Mirror { bytes } => bytes.len(),
        }
    }
}

/// One reference on a `RawOwner`, given back on drop.
pub(crate) struct Lease(RawOwner);

impl Lease {
    /// # Safety
    /// `owner` meets `RawOwner`'s contract and is live for this call.
    unsafe fn acquire(owner: RawOwner) -> Self {
        // SAFETY: the caller's contract.
        unsafe { (owner.retain)(owner.context) };
        Self(owner)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // SAFETY: this lease holds one reference, released once.
        unsafe { (self.0.release)(self.0.context) };
    }
}
