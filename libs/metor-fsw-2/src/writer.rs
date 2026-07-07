//! Serializing a frame's bytes, fixed region first and dynamic members after.
//!
//! [`FrameWriter`] builds a frame's table inside a growable [`LenPacket`]. It
//! copies the `#[repr(C)]` fixed region in first, with every dynamic slot
//! zeroed. Each [`list`](FrameWriter::list) or [`map`](FrameWriter::map) call
//! then appends that member's element block to the trailer at an 8-byte
//! boundary and patches the member's [`Slot`] with the block's offset and byte
//! length. Offsets are measured from the start of the fixed region, so one
//! trailer serves every dynamic field and a reader can resolve any slot from
//! the table base alone.
//!
//! ```text
//! | fixed region (slots patched) | pad | list elems | pad | map entries | keys |
//! offset 0                            '------------ trailer ------------------'
//! ```

use core::marker::PhantomData;
use core::mem::size_of;

use metor_proto::types::LenPacket;
use zerocopy::{Immutable, IntoBytes};

use crate::dynamic::{MapEntryHeader, Slot, map_stride, map_value_offset};
use crate::frame::Frame;

/// Byte offset of the table within `LenPacket::inner`, past the packet header
/// (4-byte length, 1 type byte, 2-byte id, 1 request-id byte).
const TABLE_BASE: usize = 8;

/// The reason [`FrameWriter::map`] rejected a key.
///
/// Map keys become path segments of the dotted component path, so a key must
/// be non-empty and must not itself contain a dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The key contained `.`, which would split it into two path segments.
    #[error("map key contains `.`, which would alias the dotted-path grammar")]
    DotInKey,
    /// The key was empty, which would vanish from the path entirely.
    #[error("map key is empty (an empty segment vanishes per the PathHasher rule)")]
    EmptyKey,
}

/// A writer that serializes one frame `F` into a contiguous table inside a
/// growable [`LenPacket`], fixed region first and dynamic members appended
/// to the trailer.
pub struct FrameWriter<F> {
    packet: LenPacket,
    /// Index in `packet.inner` where the fixed region begins.
    base: usize,
    _f: PhantomData<F>,
}

impl<F: Frame + IntoBytes + Immutable> FrameWriter<F> {
    /// Starts a writer, copying `fixed` in as the fixed region.
    ///
    /// The dynamic slots in `fixed` must be zeroed; construct them as
    /// `FrameList::EMPTY` / `FrameMap::EMPTY`.
    pub fn new(fixed: &F) -> Self {
        let packet = LenPacket::table([0, 0], F::MAX_SIZE.min(1 << 16));
        Self::from_packet(packet, fixed)
    }

    /// As [`new`](Self::new), but reuses `packet`'s allocation instead of
    /// making a fresh one, so callers can pool buffers across frames.
    ///
    /// The packet is cleared back to its header first, leaving it
    /// byte-equivalent to a newly constructed table packet.
    pub fn from_packet(mut packet: LenPacket, fixed: &F) -> Self {
        packet.clear();
        debug_assert_eq!(packet.inner.len(), TABLE_BASE);
        let base = packet.inner.len();
        packet.extend_from_slice(fixed.as_bytes());
        Self {
            packet,
            base,
            _f: PhantomData,
        }
    }

    /// Appends a list to the trailer and patches the slot at `slot_off`
    /// (obtain it with `core::mem::offset_of!`).
    pub fn list<T: IntoBytes + Immutable>(
        &mut self,
        slot_off: usize,
        build: impl FnOnce(&mut ListWriter<T>),
    ) {
        let mut lw = ListWriter::new();
        build(&mut lw);
        self.align8();
        let trailer_off = self.table_len();
        self.packet.extend_from_slice(&lw.bytes);
        self.patch_slot(slot_off, trailer_off as u32, lw.bytes.len() as u32);
    }

    /// Appends a map to the trailer as a fixed-stride entry array followed by
    /// a pool of key bytes, then patches the slot at `slot_off`.
    ///
    /// Errors if any key inserted during `build` was rejected.
    pub fn map<V: IntoBytes + Immutable>(
        &mut self,
        slot_off: usize,
        build: impl FnOnce(&mut MapWriter<V>),
    ) -> Result<(), KeyError> {
        let mut mw = MapWriter::new();
        build(&mut mw);
        if let Some(err) = mw.error {
            return Err(err);
        }

        let stride = map_stride::<V>() as usize;
        let value_offset = map_value_offset::<V>() as usize;
        let count = mw.entries.len();

        self.align8();
        let entry_array_off = self.table_len();
        let entry_array_len = count * stride;
        // The key pool sits immediately after the entry array; each entry's
        // `key_off` points table-absolute into it.
        let pool_off = entry_array_off + entry_array_len;

        let mut pool = Vec::new();
        let mut key_cursor = pool_off;
        // One entry buffer, re-zeroed each iteration, rather than an
        // allocation per entry.
        let mut entry = vec![0u8; stride];
        for (key, val) in &mw.entries {
            let kb = key.as_bytes();
            let vb = val.as_bytes();
            entry.fill(0);
            MapEntryHeader {
                key_off: key_cursor as u32,
                key_len: kb.len() as u32,
            }
            .write_to_prefix(&mut entry)
            .expect("stride >= size_of::<MapEntryHeader>()");
            entry[value_offset..value_offset + vb.len()].copy_from_slice(vb);
            self.packet.extend_from_slice(&entry);
            pool.extend_from_slice(kb);
            key_cursor += kb.len();
        }
        // The slot delimits the entry array only, so `count = byte_len / stride`.
        self.patch_slot(slot_off, entry_array_off as u32, entry_array_len as u32);
        self.packet.extend_from_slice(&pool);
        Ok(())
    }

    /// Finishes the frame, returning the backing [`LenPacket`].
    pub fn finish(self) -> LenPacket {
        self.packet
    }

    /// The raw table bytes (fixed region plus trailer), with the fixed region
    /// at offset 0.
    pub fn table(&self) -> &[u8] {
        &self.packet.inner[self.base..]
    }

    fn table_len(&self) -> usize {
        self.packet.inner.len() - self.base
    }

    fn align8(&mut self) {
        while !self.table_len().is_multiple_of(8) {
            self.packet.push(0);
        }
    }

    fn patch_slot(&mut self, slot_off: usize, trailer_off: u32, byte_len: u32) {
        let at = self.base + slot_off;
        Slot {
            trailer_off,
            byte_len,
        }
        .write_to(&mut self.packet.inner[at..at + size_of::<Slot>()])
        .expect("slot lies inside the fixed region");
    }
}

/// A staging buffer that accumulates a list's elements as raw bytes until
/// [`FrameWriter::list`] copies them back-to-back into the trailer.
pub struct ListWriter<T> {
    bytes: Vec<u8>,
    count: usize,
    _t: PhantomData<T>,
}

impl<T: IntoBytes + Immutable> ListWriter<T> {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            count: 0,
            _t: PhantomData,
        }
    }

    /// Appends one element.
    pub fn push(&mut self, elem: T) {
        debug_assert_eq!(size_of::<T>(), elem.as_bytes().len());
        self.bytes.extend_from_slice(elem.as_bytes());
        self.count += 1;
    }

    /// Number of elements pushed so far.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// A staging buffer that accumulates a map's entries until
/// [`FrameWriter::map`] serializes them into the trailer.
///
/// Keys are validated on insert and the first rejection is surfaced by
/// [`FrameWriter::map`].
pub struct MapWriter<V> {
    entries: Vec<(String, V)>,
    error: Option<KeyError>,
}

impl<V: IntoBytes + Immutable> MapWriter<V> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            error: None,
        }
    }

    /// Inserts a `(key, value)` entry, rejecting empty or `.`-containing keys.
    pub fn insert(&mut self, key: &str, value: V) {
        if let Err(e) = validate_key(key) {
            self.error.get_or_insert(e);
            return;
        }
        // Store `V` itself and emit its bytes at serialize time, so insertion
        // costs no allocation beyond the key.
        self.entries.push((key.to_string(), value));
    }
}

fn validate_key(key: &str) -> Result<(), KeyError> {
    if key.is_empty() {
        Err(KeyError::EmptyKey)
    } else if key.contains('.') {
        Err(KeyError::DotInKey)
    } else {
        Ok(())
    }
}
