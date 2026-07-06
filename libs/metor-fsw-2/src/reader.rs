//! Consumer side, typed-by-index/key access (frames.md §3.5).
//!
//! A thin presentation convenience over the same table bytes the
//! `RealizedField`/`expand_dynamic` path realizes: it reads a [`Slot`] from the
//! fixed region and indexes the trailer as `T` (list) or scans entries by key
//! (map). The authoritative dotted-id/frame/timestamp semantics remain the vtable
//! `apply` path; this reader does not re-derive them.

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
    /// Wraps the `table` bytes and the list field's `slot` (read from the fixed
    /// region).
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

    /// Element `i`, or `None` if out of range / malformed.
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

/// Reads map entries (value type `V`) out of a table trailer, keyed by name.
pub struct MapReader<'a, V> {
    table: &'a [u8],
    slot: Slot,
    _v: PhantomData<V>,
}

impl<'a, V: FromBytes> MapReader<'a, V> {
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

    /// The key and value at entry `i`.
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
        let value = V::read_from_bytes(self.table.get(value_off..value_off + size_of::<V>())?).ok()?;
        Some((key, value))
    }

    /// The value for `key`, if present.
    pub fn get(&self, key: &str) -> Option<V> {
        (0..self.len())
            .filter_map(|i| self.entry(i))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    /// Iterates `(key, value)` entries.
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, V)> + '_ {
        (0..self.len()).filter_map(|i| self.entry(i))
    }
}
