//! Writes the ABI's arguments into a guest's memory, in wasm32 layout.

use zerocopy::{FromZeros, IntoBytes};

use crate::dl::PackError;
use crate::dl::codec::DESCRIPTOR_CAPACITY;
use crate::pack::raw::{Wasm32RawRing, Wasm32RawSlice};

use super::DEFAULT_SETUP_FUEL;
use super::guest::{Guest, Out, WasmError};

/// The size and alignment of a `usize` in a wasm32 guest.
const WASM32_USIZE: u32 = 4;

/// One ring region in a guest's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GuestRing {
    pub(super) offset: u32,
    pub(super) len: u32,
}

/// One block of guest memory that [`Args`] frees when it drops.
#[derive(Clone, Copy, Debug)]
struct Allocation {
    offset: u32,
    len: u32,
    align: u32,
}

/// Writes one call's arguments into guest memory.
///
/// Arguments are allocated with the guest's own allocator, using setup fuel.
/// They are all freed when `Args` drops.
pub(super) struct Args<'g> {
    guest: &'g mut Guest,
    held: Vec<Allocation>,
}

impl<'g> Args<'g> {
    pub(super) fn new(guest: &'g mut Guest) -> Self {
        Self {
            guest,
            held: Vec::new(),
        }
    }

    /// Returns the guest, so the caller can make the call.
    pub(super) fn guest(&mut self) -> &mut Guest {
        self.guest
    }

    /// Allocates `len` zeroed bytes aligned to `align`, freed on drop.
    ///
    /// A zero `len` returns the null offset.
    fn alloc(&mut self, len: u32, align: u32) -> Result<u32, PackError> {
        if len == 0 {
            return Ok(0);
        }
        let offset = self.guest.alloc_nonnull(len, align)?;
        self.held.push(Allocation { offset, len, align });
        Ok(offset)
    }

    /// Copies `bytes` into guest memory and returns their offset.
    fn copy(&mut self, bytes: &[u8], align: u32) -> Result<u32, PackError> {
        let offset = self.alloc(guest_usize(bytes.len())?, align)?;
        self.guest.write(offset, bytes)?;
        Ok(offset)
    }

    /// Writes a `RawSlice` of `len` elements at `ptr` and returns its offset.
    pub(super) fn slice(&mut self, ptr: u32, len: u32) -> Result<u32, PackError> {
        self.copy(Wasm32RawSlice::new(ptr, len).as_bytes(), WASM32_USIZE)
    }

    /// Copies `bytes` into guest memory and returns the offset of a `RawSlice`
    /// over them.
    pub(super) fn byte_slice(&mut self, bytes: &[u8]) -> Result<u32, PackError> {
        let ptr = self.copy(bytes, 1)?;
        self.slice(ptr, guest_usize(bytes.len())?)
    }

    /// Writes an array of `RawRing`s with no owner.
    ///
    /// Returns the array's offset and length.
    pub(super) fn rings(&mut self, rings: &[GuestRing]) -> Result<(u32, u32), PackError> {
        let encoded: Vec<Wasm32RawRing> = rings
            .iter()
            .map(|ring| Wasm32RawRing::new(ring.offset, ring.len))
            .collect();
        let ptr = self.copy(encoded.as_bytes(), WASM32_USIZE)?;
        Ok((ptr, guest_usize(rings.len())?))
    }

    /// Writes a `RawRing` over `region`, or an empty one, and returns its
    /// offset, for an export to write the system's IO arena back into.
    pub(super) fn io_arena(&mut self, region: Option<GuestRing>) -> Result<u32, PackError> {
        let ring = region.map_or_else(Wasm32RawRing::new_zeroed, |region| {
            Wasm32RawRing::new(region.offset, region.len)
        });
        self.copy(ring.as_bytes(), WASM32_USIZE)
    }

    /// Reads the region of the `RawRing` at `offset`.
    pub(super) fn ring(&self, offset: u32) -> Result<GuestRing, PackError> {
        let mut ring = Wasm32RawRing::new_zeroed();
        self.guest.read(offset, ring.as_mut_bytes())?;
        Ok(GuestRing {
            offset: ring.base.get(),
            len: ring.len.get(),
        })
    }

    /// Writes an array of `RawSlice`s, one per `(ptr, len)` pair.
    ///
    /// Returns the offset of a `RawSlice` over that array.
    pub(super) fn ports(&mut self, ports: &[(u32, u32)]) -> Result<u32, PackError> {
        let encoded: Vec<Wasm32RawSlice> = ports
            .iter()
            .map(|&(ptr, len)| Wasm32RawSlice::new(ptr, len))
            .collect();
        let ptr = self.copy(encoded.as_bytes(), WASM32_USIZE)?;
        self.slice(ptr, guest_usize(ports.len())?)
    }

    /// Allocates a zeroed guest `usize`, for an export to write a handle into.
    pub(super) fn word(&mut self) -> Result<u32, PackError> {
        self.alloc(WASM32_USIZE, WASM32_USIZE)
    }

    /// Reads the guest `usize` at `offset`.
    pub(super) fn read_word(&self, offset: u32) -> Result<u32, PackError> {
        Ok(self.guest.read_u32(offset)?)
    }

    /// Allocates an output buffer for an export to write into.
    ///
    /// The buffer holds [`DESCRIPTOR_CAPACITY`] bytes, plus a slot for the
    /// length the export wrote.
    pub(super) fn out(&mut self) -> Result<Out, PackError> {
        let capacity = guest_usize(DESCRIPTOR_CAPACITY)?;
        Ok(Out {
            dst: self.alloc(capacity, 1)?,
            capacity,
            written: self.alloc(WASM32_USIZE, WASM32_USIZE)?,
        })
    }

    /// Reads back the bytes an export wrote into `out`.
    ///
    /// Fails if the export reported writing more than the buffer holds.
    pub(super) fn written(&self, out: Out) -> Result<Vec<u8>, PackError> {
        let written = self.guest.read_u32(out.written)?;
        if written > out.capacity {
            return Err(WasmError::TooLarge(written as usize).into());
        }
        let mut bytes = vec![0; written as usize];
        self.guest.read(out.dst, &mut bytes)?;
        Ok(bytes)
    }
}

impl Drop for Args<'_> {
    fn drop(&mut self) {
        for held in &self.held {
            // A trap while freeing is ignored. The guest is already unusable
            // to the caller.
            let _ = self
                .guest
                .free(held.offset, held.len, held.align, DEFAULT_SETUP_FUEL);
        }
    }
}

/// Converts a length to a guest `usize`, failing if it does not fit.
pub(super) fn guest_usize(len: usize) -> Result<u32, PackError> {
    u32::try_from(len).map_err(|_| WasmError::TooLarge(len).into())
}
