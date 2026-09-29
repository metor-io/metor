//! Writes the ABI's arguments into a guest's memory, in wasm32 layout.

use crate::dl::PackError;
use crate::dl::codec::DESCRIPTOR_CAPACITY;
use crate::pack::raw::{WASM32_RAW_RING, WASM32_RAW_SLICE};

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

/// Writes one call's arguments into guest memory.
///
/// Arguments are allocated with the guest's own allocator, using setup fuel.
/// They are all freed when `Args` drops.
pub(super) struct Args<'g> {
    guest: &'g mut Guest,
    /// Each allocation, as `(offset, len, align)`.
    held: Vec<(u32, u32, u32)>,
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
        let offset = allocated(self.guest, len, align)?;
        self.held.push((offset, len, align));
        Ok(offset)
    }

    /// Copies `bytes` into guest memory and returns their offset.
    fn copy(&mut self, bytes: &[u8], align: u32) -> Result<u32, PackError> {
        let offset = self.alloc(len(bytes.len())?, align)?;
        self.guest.write(offset, bytes)?;
        Ok(offset)
    }

    /// Writes a `RawSlice` of `len` elements at `ptr` and returns its offset.
    pub(super) fn slice(&mut self, ptr: u32, len: u32) -> Result<u32, PackError> {
        let offset = self.alloc(WASM32_RAW_SLICE as u32, WASM32_USIZE)?;
        self.guest.write(offset, &raw_slice(ptr, len))?;
        Ok(offset)
    }

    /// Copies `bytes` into guest memory and returns the offset of a `RawSlice`
    /// over them.
    pub(super) fn byte_slice(&mut self, bytes: &[u8]) -> Result<u32, PackError> {
        let ptr = self.copy(bytes, 1)?;
        self.slice(ptr, len(bytes.len())?)
    }

    /// Writes an array of `RawRing`s with no owner.
    ///
    /// Returns the array's offset and length.
    pub(super) fn rings(&mut self, rings: &[GuestRing]) -> Result<(u32, u32), PackError> {
        let encoded: Vec<u8> = rings.iter().flat_map(|ring| raw_ring(*ring)).collect();
        let ptr = self.copy(&encoded, WASM32_USIZE)?;
        Ok((ptr, len(rings.len())?))
    }

    /// Writes an array of `RawSlice`s, one per `(ptr, len)` pair.
    ///
    /// Returns the offset of a `RawSlice` over that array.
    pub(super) fn ports(&mut self, ports: &[(u32, u32)]) -> Result<u32, PackError> {
        let encoded: Vec<u8> = ports
            .iter()
            .flat_map(|&(ptr, len)| raw_slice(ptr, len))
            .collect();
        let ptr = self.copy(&encoded, WASM32_USIZE)?;
        self.slice(ptr, len(ports.len())?)
    }

    /// Allocates an output buffer for an export to write into.
    ///
    /// The buffer holds [`DESCRIPTOR_CAPACITY`] bytes, plus a slot for the
    /// length the export wrote.
    pub(super) fn out(&mut self) -> Result<Out, PackError> {
        let capacity = len(DESCRIPTOR_CAPACITY)?;
        Ok(Out {
            dst: self.alloc(capacity, 1)?,
            capacity,
            written: self.alloc(WASM32_USIZE, WASM32_USIZE)?,
        })
    }

    /// Reads back the bytes an export wrote into `out`.
    ///
    /// Returns `None` if the export reported writing more than the buffer
    /// holds.
    pub(super) fn written(&self, out: Out) -> Result<Option<Vec<u8>>, PackError> {
        let written = read_u32(self.guest, out.written)?;
        if written > out.capacity {
            return Ok(None);
        }
        let mut bytes = vec![0; written as usize];
        self.guest.read(out.dst, &mut bytes)?;
        Ok(Some(bytes))
    }

    /// Reads the bytes of the `RawSlice` at `offset`.
    ///
    /// Fails if the slice is longer than [`DESCRIPTOR_CAPACITY`].
    pub(super) fn slice_bytes(&self, offset: u32) -> Result<Vec<u8>, PackError> {
        let ptr = read_u32(self.guest, offset)?;
        let len = read_u32(self.guest, offset.saturating_add(WASM32_USIZE))? as usize;
        if len > DESCRIPTOR_CAPACITY {
            return Err(WasmError::TooLarge(len).into());
        }
        let mut bytes = vec![0; len];
        self.guest.read(ptr, &mut bytes)?;
        Ok(bytes)
    }
}

impl Drop for Args<'_> {
    fn drop(&mut self) {
        for &(offset, len, align) in &self.held {
            // A trap while freeing is ignored. The guest is already unusable
            // to the caller.
            let _ = self.guest.free(offset, len, align, DEFAULT_SETUP_FUEL);
        }
    }
}

/// Allocates `len` bytes with the guest's allocator, using setup fuel.
///
/// Fails if the allocator returns the null offset.
pub(super) fn allocated(guest: &mut Guest, len: u32, align: u32) -> Result<u32, PackError> {
    match guest.alloc(len, align, DEFAULT_SETUP_FUEL)? {
        0 => Err(WasmError::Alloc(len).into()),
        offset => Ok(offset),
    }
}

/// Converts a length to a guest `usize`, failing if it does not fit.
fn len(len: usize) -> Result<u32, PackError> {
    u32::try_from(len).map_err(|_| WasmError::TooLarge(len).into())
}

/// Reads the little-endian `u32` at `offset`.
fn read_u32(guest: &Guest, offset: u32) -> Result<u32, PackError> {
    let mut word = [0; 4];
    guest.read(offset, &mut word)?;
    Ok(u32::from_le_bytes(word))
}

/// Encodes a `RawSlice` in wasm32 layout: `ptr`, then `len`.
pub(super) fn raw_slice(ptr: u32, len: u32) -> [u8; WASM32_RAW_SLICE] {
    let mut bytes = [0; WASM32_RAW_SLICE];
    bytes[..4].copy_from_slice(&ptr.to_le_bytes());
    bytes[4..].copy_from_slice(&len.to_le_bytes());
    bytes
}

/// Encodes a `RawRing` in wasm32 layout: `base`, `len`, then a `None` owner.
pub(super) fn raw_ring(ring: GuestRing) -> [u8; WASM32_RAW_RING] {
    let mut bytes = [0; WASM32_RAW_RING];
    bytes[..WASM32_RAW_SLICE].copy_from_slice(&raw_slice(ring.offset, ring.len));
    bytes
}
