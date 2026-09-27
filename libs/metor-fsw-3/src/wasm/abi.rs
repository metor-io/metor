//! The ABI's arguments, placed in a guest's memory in wasm32 layout.

use crate::dl::PackError;
use crate::dl::codec::DESCRIPTOR_CAPACITY;
use crate::pack::raw::WASM32_RAW_SLICE;

use super::guest::{Guest, Out, WasmError};

/// A `usize`'s size and alignment in a wasm32 guest.
const WASM32_USIZE: u32 = 4;

/// `Args` places one call's arguments in a guest's memory through its
/// allocator, and frees them all when dropped.
pub(super) struct Args<'g> {
    guest: &'g mut Guest,
    fuel: u64,
    /// Each allocation as `(offset, len, align)`.
    held: Vec<(u32, u32, u32)>,
}

impl<'g> Args<'g> {
    /// Calls into `guest` for its allocator under a grant of `fuel` each.
    pub(super) fn new(guest: &'g mut Guest, fuel: u64) -> Self {
        Self {
            guest,
            fuel,
            held: Vec::new(),
        }
    }

    /// The guest, for the call these arguments are for.
    pub(super) fn guest(&mut self) -> &mut Guest {
        self.guest
    }

    /// `len` zeroed bytes aligned to `align`, held until drop. No bytes is
    /// the null offset.
    pub(super) fn alloc(&mut self, len: u32, align: u32) -> Result<u32, PackError> {
        if len == 0 {
            return Ok(0);
        }
        let offset = self.guest.alloc(len, align, self.fuel)?;
        if offset == 0 {
            return Err(WasmError::Alloc(len).into());
        }
        self.held.push((offset, len, align));
        Ok(offset)
    }

    /// A copy of `bytes`, returning its offset.
    pub(super) fn bytes(&mut self, bytes: &[u8]) -> Result<u32, PackError> {
        let offset = self.alloc(len(bytes.len())?, 1)?;
        self.guest.write(offset, bytes)?;
        Ok(offset)
    }

    /// A `RawSlice` of `len` elements at `ptr`, returning its offset.
    pub(super) fn slice(&mut self, ptr: u32, len: u32) -> Result<u32, PackError> {
        let offset = self.alloc(WASM32_RAW_SLICE as u32, WASM32_USIZE)?;
        self.guest.write(offset, &raw_slice(ptr, len))?;
        Ok(offset)
    }

    /// A `RawSlice` over a copy of `bytes`, returning the slice's offset.
    pub(super) fn byte_slice(&mut self, bytes: &[u8]) -> Result<u32, PackError> {
        let ptr = self.bytes(bytes)?;
        self.slice(ptr, len(bytes.len())?)
    }

    /// A [`DESCRIPTOR_CAPACITY`] buffer and the slot its length goes in.
    pub(super) fn out(&mut self) -> Result<Out, PackError> {
        let capacity = len(DESCRIPTOR_CAPACITY)?;
        Ok(Out {
            dst: self.alloc(capacity, 1)?,
            capacity,
            written: self.alloc(WASM32_USIZE, WASM32_USIZE)?,
        })
    }

    /// The bytes an export reports writing into `out`, `None` when that is
    /// past the buffer.
    pub(super) fn written(&self, out: Out) -> Result<Option<Vec<u8>>, PackError> {
        let written = read_u32(self.guest, out.written)?;
        if written > out.capacity {
            return Ok(None);
        }
        let mut bytes = vec![0; written as usize];
        self.guest.read(out.dst, &mut bytes)?;
        Ok(Some(bytes))
    }
}

impl Drop for Args<'_> {
    fn drop(&mut self) {
        for &(offset, len, align) in &self.held {
            // A guest that traps here is already dead to the caller.
            let _ = self.guest.free(offset, len, align, self.fuel);
        }
    }
}

/// A length as a guest `usize`.
fn len(len: usize) -> Result<u32, PackError> {
    u32::try_from(len).map_err(|_| WasmError::TooLarge(len).into())
}

/// The little-endian `u32` at `offset`.
fn read_u32(guest: &Guest, offset: u32) -> Result<u32, PackError> {
    let mut word = [0; 4];
    guest.read(offset, &mut word)?;
    Ok(u32::from_le_bytes(word))
}

/// A `RawSlice` in wasm32 layout: `ptr`, then `len`.
pub(super) fn raw_slice(ptr: u32, len: u32) -> [u8; WASM32_RAW_SLICE] {
    let mut bytes = [0; WASM32_RAW_SLICE];
    bytes[..4].copy_from_slice(&ptr.to_le_bytes());
    bytes[4..].copy_from_slice(&len.to_le_bytes());
    bytes
}
