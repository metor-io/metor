//! Typed read access to a frame's dynamic fields.
//!
//! A list or map field occupies an 8-byte [`Slot`] in the frame's fixed
//! region, and its element bytes live in the trailer that follows. The
//! readers here wrap the full table bytes plus one such slot and decode
//! elements on demand, by index for lists and by key for maps. Every access
//! is bounds checked against the table, so a truncated or corrupt trailer
//! yields `None` rather than a panic, and iterators simply skip entries
//! whose bytes are out of range.
//!
//! These readers are a presentation convenience. The frame's vtable remains
//! the authoritative interpretation of the table; nothing here re-derives
//! field identity or timestamp semantics.

use core::marker::PhantomData;
use core::mem::size_of;

use zerocopy::FromBytes;

use crate::dynamic::{MapEntryHeader, Slot, map_stride, map_value_offset};

/// Reads list elements of type `T` out of a table trailer.
pub struct ListReader<'a, T> {
    table: &'a [u8],
    slot: Slot,
    _t: PhantomData<T>,
}

impl<'a, T: FromBytes> ListReader<'a, T> {
    /// Wraps the `table` bytes and a list field's `slot` from the fixed
    /// region.
    pub fn new(table: &'a [u8], slot: Slot) -> Self {
        Self {
            table,
            slot,
            _t: PhantomData,
        }
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        self.slot.byte_len as usize / size_of::<T>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Element `i`, or `None` if it is out of range or its bytes fall
    /// outside the table.
    pub fn get(&self, i: usize) -> Option<T> {
        if i >= self.len() {
            return None;
        }
        let start = self.slot.trailer_off as usize + i * size_of::<T>();
        let bytes = self.table.get(start..start + size_of::<T>())?;
        T::read_from_bytes(bytes).ok()
    }

    /// Iterates the elements.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        (0..self.len()).filter_map(|i| self.get(i))
    }
}

/// Reads map entries with values of type `V` out of a table trailer.
pub struct MapReader<'a, V> {
    table: &'a [u8],
    slot: Slot,
    _v: PhantomData<V>,
}

impl<'a, V: FromBytes> MapReader<'a, V> {
    /// Wraps the `table` bytes and a map field's `slot` from the fixed
    /// region.
    pub fn new(table: &'a [u8], slot: Slot) -> Self {
        Self {
            table,
            slot,
            _v: PhantomData,
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.slot.byte_len as usize / map_stride::<V>() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The key and value at entry `i`, or `None` if it is out of range or
    /// malformed.
    pub fn entry(&self, i: usize) -> Option<(&'a str, V)> {
        if i >= self.len() {
            return None;
        }
        let stride = map_stride::<V>() as usize;
        let entry_base = self.slot.trailer_off as usize + i * stride;
        let (hdr, _) = MapEntryHeader::read_from_prefix(self.table.get(entry_base..)?).ok()?;
        let key_off = hdr.key_off as usize;
        let key_len = hdr.key_len as usize;
        let key = core::str::from_utf8(self.table.get(key_off..key_off + key_len)?).ok()?;
        let value_off = entry_base + map_value_offset::<V>() as usize;
        let value =
            V::read_from_bytes(self.table.get(value_off..value_off + size_of::<V>())?).ok()?;
        Some((key, value))
    }

    /// The value for `key`, found by a linear scan of the entries.
    pub fn get(&self, key: &str) -> Option<V> {
        (0..self.len())
            .filter_map(|i| self.entry(i))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    /// Iterates the `(key, value)` entries.
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, V)> + '_ {
        (0..self.len()).filter_map(|i| self.entry(i))
    }
}
