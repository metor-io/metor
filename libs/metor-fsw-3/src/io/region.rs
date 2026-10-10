//! Arena memory that another party owns, or that this side mirrors.

use core::ptr::NonNull;

use metor_fsw_3_ring::RawOwner;

/// A buffer table's memory when it is not the table's own allocation.
pub(crate) enum Region {
    /// Memory another party owns, such as a dylib's arena. The lease keeps
    /// it alive; without one, the memory lives as long as this side does.
    Shared {
        base: NonNull<u8>,
        len: usize,
        lease: Option<Lease>,
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
        let lease = Some(unsafe { Lease::acquire(owner) });
        Self::Shared { base, len, lease }
    }

    /// The region at `base..base + len`, with no owner.
    ///
    /// # Safety
    /// The `len` bytes at `base` stay alive and unmoved for as long as the
    /// region does, such as memory a wasm host wrote into its own guest, or a
    /// guest's memory seen by its host.
    pub(crate) unsafe fn unowned(base: NonNull<u8>, len: usize) -> Self {
        Self::Shared {
            base,
            len,
            lease: None,
        }
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

    /// The owner a `Shared` region was given. A mirror has none.
    pub(crate) fn owner(&self) -> Option<RawOwner> {
        match self {
            Self::Shared { lease, .. } => lease.as_ref().map(|l| l.0),
            Self::Mirror { .. } => None,
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
