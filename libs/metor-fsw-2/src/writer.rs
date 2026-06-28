//! Producer side: build a frame's bytes (fixed region + trailer) with dynamic
//! members patched in (frames.md §3.3).
//!
//! [`FrameWriter`] wraps a growable [`LenPacket`]. The fixed `#[repr(C)]` region is
//! written first (slots zeroed), then each dynamic field appends its element block
//! to the trailer — 8-byte aligned, reusing the ring's [`round_up8`] alignment — and
//! patches its slot `{ trailer_off, byte_len }`. All trailer offsets are
//! table-absolute (relative to the fixed-region start), matching the
//! one-global-trailer invariant.

use core::marker::PhantomData;
use core::mem::size_of;

use metor_proto::types::LenPacket;
use zerocopy::{Immutable, IntoBytes};

use crate::dynamic::{map_stride, map_value_offset};
use crate::frame::Frame;

/// Bytes start of the table within a `LenPacket::table` (4-byte length + 1 ty +
/// 2 id + 1 req_id).
const TABLE_BASE: usize = 8;

/// Errors raised while building a frame's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// A map key contained `.`, which would alias the dotted-path grammar.
    DotInKey,
    /// A map key was empty (an empty segment vanishes per the `PathHasher` rule).
    EmptyKey,
}

/// Builds the table bytes for a frame `F` over a [`LenPacket`].
pub struct FrameWriter<F> {
    packet: LenPacket,
    /// Index in `packet.inner` where the table (fixed region) begins.
    base: usize,
    _f: PhantomData<F>,
}

impl<F: Frame + IntoBytes + Immutable> FrameWriter<F> {
    /// Starts a writer, seeding the fixed region from `fixed` (with its dynamic
    /// slots zeroed — construct them as `FrameList::EMPTY` / `FrameMap::EMPTY`).
    pub fn new(fixed: &F) -> Self {
        let packet = LenPacket::table([0, 0], F::MAX_SIZE.min(1 << 16));
        Self::from_packet(packet, fixed)
    }

    /// As [`new`](Self::new) but reuses an existing [`LenPacket`] buffer (cleared
    /// back to the table base) instead of allocating a fresh one — the per-write
    /// pooling path used by [`Output::write_with`](crate::Output::write_with) to
    /// avoid a heap alloc+free on every dynamic-member publish (e.g. per-cycle
    /// health). The reused buffer is byte-equivalent to a fresh
    /// `LenPacket::table([0, 0], _)` after `clear()`.
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

    /// Appends a list to the trailer and patches the slot at `slot_off` (use
    /// `core::mem::offset_of!`).
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

    /// Appends a map to the trailer (fixed-stride entry array followed by a name
    /// pool) and patches the slot at `slot_off`. Returns an error if any inserted
    /// key was rejected (frames.md §3.3, §Q5).
    pub fn map<V: IntoBytes + Immutable>(
        &mut self,
        slot_off: usize,
        build: impl FnOnce(&mut MapWriter<V>),
    ) -> Result<(), WriteError> {
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
        // The name pool sits immediately after the entry array; `key_off` points
        // table-absolute into it.
        let pool_off = entry_array_off + entry_array_len;

        let mut pool = Vec::new();
        let mut key_cursor = pool_off;
        // One reused entry buffer (zeroed each iteration) rather than a heap
        // allocation per map entry.
        let mut entry = vec![0u8; stride];
        for (key, val) in &mw.entries {
            let kb = key.as_bytes();
            let vb = val.as_bytes();
            entry.fill(0);
            entry[0..4].copy_from_slice(&(key_cursor as u32).to_le_bytes());
            entry[4..8].copy_from_slice(&(kb.len() as u32).to_le_bytes());
            entry[value_offset..value_offset + vb.len()].copy_from_slice(vb);
            self.packet.extend_from_slice(&entry);
            pool.extend_from_slice(kb);
            key_cursor += kb.len();
        }
        // The slot delimits the entry array only (so `count = byte_len / stride`).
        self.patch_slot(slot_off, entry_array_off as u32, entry_array_len as u32);
        self.packet.extend_from_slice(&pool);
        Ok(())
    }

    /// Finishes the frame, returning the backing [`LenPacket`].
    pub fn finish(self) -> LenPacket {
        self.packet
    }

    /// The raw table bytes (fixed region + trailer), with offset 0 at the fixed
    /// region — feed this to `VTable::apply`.
    pub fn table(&self) -> &[u8] {
        &self.packet.inner[self.base..]
    }

    fn table_len(&self) -> usize {
        self.packet.inner.len() - self.base
    }

    fn align8(&mut self) {
        while self.table_len() % 8 != 0 {
            self.packet.push(0);
        }
    }

    fn patch_slot(&mut self, slot_off: usize, trailer_off: u32, byte_len: u32) {
        let at = self.base + slot_off;
        self.packet.inner[at..at + 4].copy_from_slice(&trailer_off.to_le_bytes());
        self.packet.inner[at + 4..at + 8].copy_from_slice(&byte_len.to_le_bytes());
    }
}

/// Records list elements before they are serialized back-to-back into the trailer.
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

    /// Appends one element (its `#[repr(C)]` bytes).
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

/// Records map entries before they are serialized into the trailer. Keys are
/// validated on insert; the first rejection is surfaced by [`FrameWriter::map`].
pub struct MapWriter<V> {
    entries: Vec<(String, V)>,
    error: Option<WriteError>,
}

impl<V: IntoBytes + Immutable> MapWriter<V> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            error: None,
        }
    }

    /// Inserts a `(key, value)` entry, rejecting `.`-containing or empty keys.
    pub fn insert(&mut self, key: &str, value: V) {
        if let Err(e) = validate_key(key) {
            self.error.get_or_insert(e);
            return;
        }
        // Store `V` directly; its bytes are emitted at serialize time, avoiding a
        // per-entry heap allocation here.
        self.entries.push((key.to_string(), value));
    }
}

fn validate_key(key: &str) -> Result<(), WriteError> {
    if key.is_empty() {
        Err(WriteError::EmptyKey)
    } else if key.contains('.') {
        Err(WriteError::DotInKey)
    } else {
        Ok(())
    }
}
