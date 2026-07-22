//! Serializing a frame's bytes, fixed region first and dynamic members after.
//!
//! [`FrameWriter`] builds a frame's table inside a growable [`LenPacket`]. It
//! copies the `#[repr(C)]` fixed region in first. The caller must set each
//! dynamic member to its `EMPTY` value. Each [`list`](FrameWriter::list) or
//! [`map`](FrameWriter::map) call
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
//!
//! The builders write straight into the packet — a pushed list element or
//! inserted map entry lands in the trailer as the call returns. The one thing
//! that cannot land immediately is a map's key pool: it sits after the entry
//! array, whose length is unknown until the build closure finishes, so key
//! bytes stage in a small buffer and the entry headers carry pool-relative
//! offsets that [`map`](FrameWriter::map) rebases once the pool position is
//! known. Both the packet and that staging buffer travel together as a
//! [`FrameScratch`], which `Output::write_with` pools per port — a
//! steady-state publish of a dynamic frame allocates nothing.

use core::marker::PhantomData;
use core::mem::size_of;

use metor_proto::types::LenPacket;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::dynamic::{FrameList, FrameMap, MapEntryHeader, Slot, map_stride, map_value_offset};
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

/// A dynamic member exceeded the bounds used to compute its frame's
/// [`Componentize::MAX_SIZE`](metor_fsw::Componentize::MAX_SIZE). The member
/// is rolled back before this is returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DynamicWriteError {
    #[error("frame list exceeds its {max}-element bound")]
    ListFull { max: usize },
    #[error("frame map exceeds its {max}-entry bound")]
    MapFull { max: usize },
    #[error("frame map key is {len} bytes, exceeding its {max}-byte bound")]
    KeyTooLong { len: usize, max: usize },
    #[error(transparent)]
    Key(#[from] KeyError),
}

/// The recycled backing a [`FrameWriter`] builds into: the table packet plus
/// the map-key staging buffer. `Output::write_with` pools one per port and
/// threads it through [`FrameWriter::from_scratch`]/[`FrameWriter::finish`],
/// so the allocations converge to the largest frame written and then recycle.
pub struct FrameScratch {
    packet: LenPacket,
    keys: Vec<u8>,
}

impl FrameScratch {
    /// A fresh backing whose packet reserves `cap` bytes.
    pub fn new(cap: usize) -> Self {
        Self {
            packet: LenPacket::table([0, 0], cap),
            keys: Vec::new(),
        }
    }
}

/// A writer that serializes one frame `F` into a contiguous table inside a
/// growable [`LenPacket`], fixed region first and dynamic members appended
/// to the trailer.
pub struct FrameWriter<F> {
    packet: LenPacket,
    /// Index in `packet.inner` where the fixed region begins.
    base: usize,
    /// Staging for the in-progress map member's key bytes; emptied back into
    /// the trailer when the member finishes, so it holds bytes only while a
    /// [`map`](Self::map) call runs.
    keys: Vec<u8>,
    error: Option<DynamicWriteError>,
    _f: PhantomData<F>,
}

impl<F: Frame + IntoBytes + Immutable> FrameWriter<F> {
    /// Starts a writer over fresh allocations, copying `fixed` in as the
    /// fixed region.
    ///
    /// The dynamic slots in `fixed` must be zeroed; construct them as
    /// `FrameList::EMPTY` / `FrameMap::EMPTY`.
    pub fn new(fixed: &F) -> Self {
        Self::from_scratch(FrameScratch::new(F::MAX_SIZE.min(1 << 16)), fixed)
    }

    /// As [`new`](Self::new), but reuses `scratch`'s allocations instead of
    /// making fresh ones, so callers can pool the backing across frames.
    ///
    /// The packet is cleared back to its header first, leaving it
    /// byte-equivalent to a newly constructed table packet.
    pub fn from_scratch(scratch: FrameScratch, fixed: &F) -> Self {
        let FrameScratch {
            mut packet,
            mut keys,
        } = scratch;
        packet.clear();
        keys.clear();
        debug_assert_eq!(packet.inner.len(), TABLE_BASE);
        let base = packet.inner.len();
        packet.extend_from_slice(fixed.as_bytes());
        Self {
            packet,
            base,
            keys,
            error: None,
            _f: PhantomData,
        }
    }

    /// Appends a list to the trailer and patches the slot at `slot_off`
    /// (obtain it with `core::mem::offset_of!`). Elements land in the trailer
    /// as they are pushed.
    pub fn list<T: IntoBytes + Immutable, const MAX: usize>(
        &mut self,
        _field: &FrameList<T, MAX>,
        slot_off: usize,
        build: impl FnOnce(&mut ListWriter<'_, T, MAX>),
    ) -> Result<(), DynamicWriteError> {
        self.align8();
        let trailer_off = self.table_len();
        let mut lw = ListWriter {
            packet: &mut self.packet,
            count: 0,
            error: None,
            _t: PhantomData,
        };
        build(&mut lw);
        if let Some(error) = lw.error {
            self.truncate_table(trailer_off);
            self.error.get_or_insert(error);
            return Err(error);
        }
        let byte_len = self.table_len() - trailer_off;
        self.patch_slot(slot_off, trailer_off as u32, byte_len as u32);
        Ok(())
    }

    /// Appends a map to the trailer as a fixed-stride entry array followed by
    /// a pool of key bytes, then patches the slot at `slot_off`.
    ///
    /// Errors if any key inserted during `build` was rejected, rolling the
    /// whole member back so the slot stays empty.
    pub fn map<V: IntoBytes + Immutable, const MAX: usize, const MAX_KEY: usize>(
        &mut self,
        _field: &FrameMap<V, MAX, MAX_KEY>,
        slot_off: usize,
        build: impl FnOnce(&mut MapWriter<'_, V, MAX, MAX_KEY>),
    ) -> Result<(), DynamicWriteError> {
        self.align8();
        let entry_array_off = self.table_len();
        let keys_mark = self.keys.len();
        let mut mw = MapWriter {
            packet: &mut self.packet,
            keys: &mut self.keys,
            keys_mark,
            count: 0,
            error: None,
            _v: PhantomData,
        };
        build(&mut mw);
        let (count, error) = (mw.count, mw.error);
        if let Some(err) = error {
            // Roll the member back — entries out of the trailer, staged keys
            // out of the buffer — so the slot stays zeroed, as if the call
            // never ran.
            self.truncate_table(entry_array_off);
            self.keys.truncate(keys_mark);
            self.error.get_or_insert(err);
            return Err(err);
        }

        let stride = map_stride::<V>() as usize;
        let entry_array_len = count * stride;
        // The key pool sits immediately after the entry array. The staged
        // headers carry pool-relative key offsets (the pool position was
        // unknown while entries were appended); rebase them now that it is.
        let pool_off = entry_array_off + entry_array_len;
        for i in 0..count {
            let at = self.base + entry_array_off + i * stride;
            let (header, _) = MapEntryHeader::mut_from_prefix(&mut self.packet.inner[at..])
                .expect("entry array lies inside the packet");
            header.key_off += pool_off as u32;
        }

        // The slot delimits the entry array only, so `count = byte_len / stride`.
        self.patch_slot(slot_off, entry_array_off as u32, entry_array_len as u32);
        let keys = &self.keys[keys_mark..];
        self.packet.extend_from_slice(keys);
        self.keys.truncate(keys_mark);
        Ok(())
    }

    /// The first dynamic-member error raised while building this frame.
    pub fn error(&self) -> Option<DynamicWriteError> {
        self.error
    }

    /// Finishes the frame, handing the pooled backing back to the caller.
    pub fn finish(self) -> FrameScratch {
        FrameScratch {
            packet: self.packet,
            keys: self.keys,
        }
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
        let len = self.table_len();
        pad_zero(&mut self.packet, len.next_multiple_of(8) - len);
    }

    /// Truncates the table back to `table_len` bytes, keeping the packet's
    /// length prefix consistent (the prefix counts everything after itself).
    fn truncate_table(&mut self, table_len: usize) {
        let end = self.base + table_len;
        self.packet.inner.truncate(end);
        let len = (end - size_of::<u32>()) as u32;
        self.packet.inner[..size_of::<u32>()].copy_from_slice(&len.to_le_bytes());
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

/// Zero-fill `n` bytes of padding.
fn pad_zero(packet: &mut LenPacket, mut n: usize) {
    const ZERO: [u8; 8] = [0; 8];
    while n > 0 {
        let k = n.min(ZERO.len());
        packet.extend_from_slice(&ZERO[..k]);
        n -= k;
    }
}

/// Appends a list's elements straight into the trailer;
/// [`FrameWriter::list`] measures the block around the build closure and
/// patches the slot afterwards.
pub struct ListWriter<'a, T, const MAX: usize> {
    packet: &'a mut LenPacket,
    count: usize,
    error: Option<DynamicWriteError>,
    _t: PhantomData<T>,
}

impl<T: IntoBytes + Immutable, const MAX: usize> ListWriter<'_, T, MAX> {
    /// Appends one element.
    pub fn push(&mut self, elem: T) {
        if self.error.is_some() {
            return;
        }
        if self.count == MAX {
            self.error = Some(DynamicWriteError::ListFull { max: MAX });
            return;
        }
        debug_assert_eq!(size_of::<T>(), elem.as_bytes().len());
        self.packet.extend_from_slice(elem.as_bytes());
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

/// Appends a map's entries straight into the trailer as they are inserted,
/// staging only the key bytes (their pool position is unknown until the
/// build closure finishes).
///
/// Keys are validated on insert and the first rejection is surfaced by
/// [`FrameWriter::map`], which rolls the whole member back.
pub struct MapWriter<'a, V, const MAX: usize, const MAX_KEY: usize> {
    packet: &'a mut LenPacket,
    keys: &'a mut Vec<u8>,
    /// The staging length at member start; header key offsets are stored
    /// relative to it until the member's pool position is known.
    keys_mark: usize,
    count: usize,
    error: Option<DynamicWriteError>,
    _v: PhantomData<V>,
}

impl<V: IntoBytes + Immutable, const MAX: usize, const MAX_KEY: usize>
    MapWriter<'_, V, MAX, MAX_KEY>
{
    /// Inserts a `(key, value)` entry, rejecting empty or `.`-containing keys.
    pub fn insert(&mut self, key: &str, value: V) {
        if self.error.is_some() {
            return;
        }
        if let Err(e) = validate_key(key) {
            self.error = Some(e.into());
            return;
        }
        if self.count == MAX {
            self.error = Some(DynamicWriteError::MapFull { max: MAX });
            return;
        }
        let kb = key.as_bytes();
        if kb.len() > MAX_KEY {
            self.error = Some(DynamicWriteError::KeyTooLong {
                len: kb.len(),
                max: MAX_KEY,
            });
            return;
        }
        let value_offset = map_value_offset::<V>() as usize;
        let stride = map_stride::<V>() as usize;
        // One fixed-stride entry: the header (with its key offset still
        // pool-relative), padding up to the value's alignment, the value
        // bytes, padding out to the stride.
        let header = MapEntryHeader {
            key_off: (self.keys.len() - self.keys_mark) as u32,
            key_len: kb.len() as u32,
        };
        self.packet.extend_from_slice(header.as_bytes());
        pad_zero(self.packet, value_offset - size_of::<MapEntryHeader>());
        self.packet.extend_from_slice(value.as_bytes());
        pad_zero(self.packet, stride - value_offset - size_of::<V>());
        self.keys.extend_from_slice(kb);
        self.count += 1;
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
