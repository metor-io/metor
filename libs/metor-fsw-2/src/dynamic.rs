//! Runtime-sized frame members: [`FrameList`] and [`FrameMap`] (frames.md §3).
//!
//! In the `#[repr(C)]` frame, a list/map field **is** the 8-byte [`Slot`]
//! `{ trailer_off, byte_len }` — nothing more. The element data lives in the table
//! trailer past the fixed region, exactly as `expand_dynamic` reads it.
//! The const generics carry the max cardinality (and, for maps, the max key length)
//! so `Componentize::MAX_SIZE` is a `const` expression (frames.md §3.4).

use core::marker::PhantomData;
use core::mem::{align_of, size_of};

use metor_fsw::path::ComponentPath;
use metor_fsw::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use metor_proto::vtable::builder::{FieldBuilder, list, map, raw_field};
use metor_proto_wkt::ComponentMetadata;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// The 8-byte dynamic-field slot in a frame's fixed region: a table-absolute offset
/// to the element block in the trailer and its byte length. Matches the layout
/// `read_slot` reads.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Slot {
    pub trailer_off: u32,
    pub byte_len: u32,
}

/// The 8-byte `{ key_off, key_len }` prefix of a map entry (frames.md §3.3):
/// a table-absolute offset into the key pool and the key's byte length.
/// Stored native-endian like every other frame field — the format is de facto
/// little-endian (`Slot` is written by `patch_slot` and read back native by
/// `Slot::read_from_prefix`, identical on every supported target; the ring's
/// `arch_tag` rejects cross-endian producers, and metor-proto's vtable
/// interpreter reads these fields as LE).
#[repr(C)]
#[derive(Clone, Copy, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub(crate) struct MapEntryHeader {
    pub key_off: u32,
    pub key_len: u32,
}

const _: () = assert!(size_of::<MapEntryHeader>() == 8);

/// Round `n` up to a multiple of `a` (a power of two).
pub(crate) const fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

/// Pack a string into a fixed `N`-byte buffer plus its used length (truncating) —
/// the ONE packing helper behind every fixed-size name/message field in the host
/// frames (`LogLine`, `ProgressLine`, the coordinator/slot status names — C4).
pub(crate) fn pack_str<const N: usize>(s: &str) -> ([u8; N], u8) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(N);
    let mut buf = [0u8; N];
    buf[..n].copy_from_slice(&bytes[..n]);
    (buf, n as u8)
}

/// Entry alignment for a map value type — at least 8 so each entry's `key_off`/
/// `key_len` pair and value stay 8-byte aligned in the trailer.
pub(crate) const fn entry_align<V>() -> usize {
    let a = align_of::<V>();
    if a < 8 { 8 } else { a }
}

/// Byte offset of the value sub-frame within a map entry `{ key_off, key_len, value }`.
pub(crate) const fn map_value_offset<V>() -> u32 {
    align_up(size_of::<MapEntryHeader>(), entry_align::<V>()) as u32
}

/// Byte stride of one map entry (entry array element).
pub(crate) const fn map_stride<V>() -> u32 {
    align_up(map_value_offset::<V>() as usize + size_of::<V>(), entry_align::<V>()) as u32
}

/// A runtime-sized, positionally-indexed sequence of element frames
/// (`processes.0.pid`). `MAX` bounds the element count for buffer sizing.
///
/// `#[repr(transparent)]` over the [`Slot`] (the `PhantomData` is a ZST), so the
/// field is exactly the 8-byte slot and stays trivially `IntoBytes`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FrameList<T, const MAX: usize> {
    slot: Slot,
    _ty: PhantomData<T>,
}

impl<T, const MAX: usize> Default for FrameList<T, MAX> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<T, const MAX: usize> FrameList<T, MAX> {
    /// An empty list slot (`trailer_off = 0`, `byte_len = 0`). The producer patches
    /// it via the [`FrameWriter`](crate::writer::FrameWriter).
    pub const EMPTY: Self = Self {
        slot: Slot { trailer_off: 0, byte_len: 0 },
        _ty: PhantomData,
    };

    /// The raw slot.
    pub const fn slot(&self) -> Slot {
        self.slot
    }
}

impl<T: AsVTable, const MAX: usize> AsVTable for FrameList<T, MAX> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        // Reached statically: the list's `Op::List` name is the full dotted prefix
        // (frames.md §4). The slot is at offset 0; the enclosing frame `offset_by`s it.
        let prefix = path.to_name();
        core::iter::once(raw_field(
            0,
            size_of::<Slot>() as u32,
            list(&prefix, T::element_fields(String::new()), size_of::<T>() as u32),
        ))
    }

    fn element_fields(prefix: String) -> impl Iterator<Item = FieldBuilder> {
        // Reached as a member template: the list name is the relative own name only.
        core::iter::once(raw_field(
            0,
            size_of::<Slot>() as u32,
            list(&prefix, T::element_fields(String::new()), size_of::<T>() as u32),
        ))
    }
}

impl<T, const MAX: usize> Componentize for FrameList<T, MAX> {
    // The slot carries no in-struct value, so there is nothing to sink directly;
    // elements are sunk through the vtable/trailer path.
    fn sink_columns(&self, _output: &mut impl Decomponentize) {}

    // Trailer budget: `MAX` elements at the element stride, padded to 8 bytes.
    const MAX_SIZE: usize = metor_fsw_ring::round_up8(MAX * size_of::<T>());
}

impl<T, const MAX: usize> Decomponentize for FrameList<T, MAX> {
    type Error = core::convert::Infallible;
    fn apply_value(
        &mut self,
        _component_id: ComponentId,
        _view: ComponentView<'_>,
        _timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<T, const MAX: usize> Metadatatize for FrameList<T, MAX> {
    fn metadata(_prefix: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        // Dynamic members are announced lazily, per new key, by the producer (§5).
        core::iter::empty()
    }
}

/// A runtime-sized, name-keyed sequence of element frames (`processes.htop.pid`).
/// `MAX` bounds the entry count and `MAX_KEY` the per-key length for buffer sizing.
/// Keys are `&str`s, validated at write time by [`MapWriter::insert`]
/// (`crate::MapWriter`) — no `.`, non-empty — so there is no key type parameter
/// (the former phantom `K` carried no behavior — C5).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FrameMap<V, const MAX: usize, const MAX_KEY: usize = 32> {
    slot: Slot,
    _kv: PhantomData<V>,
}

impl<V, const MAX: usize, const MAX_KEY: usize> Default for FrameMap<V, MAX, MAX_KEY> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<V, const MAX: usize, const MAX_KEY: usize> FrameMap<V, MAX, MAX_KEY> {
    /// An empty map slot.
    pub const EMPTY: Self = Self {
        slot: Slot { trailer_off: 0, byte_len: 0 },
        _kv: PhantomData,
    };

    /// The raw slot.
    pub const fn slot(&self) -> Slot {
        self.slot
    }
}

impl<V: AsVTable, const MAX: usize, const MAX_KEY: usize> AsVTable
    for FrameMap<V, MAX, MAX_KEY>
{
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let prefix = path.to_name();
        core::iter::once(raw_field(
            0,
            size_of::<Slot>() as u32,
            map(
                &prefix,
                V::element_fields(String::new()),
                map_stride::<V>(),
                map_value_offset::<V>(),
            ),
        ))
    }

    fn element_fields(prefix: String) -> impl Iterator<Item = FieldBuilder> {
        core::iter::once(raw_field(
            0,
            size_of::<Slot>() as u32,
            map(
                &prefix,
                V::element_fields(String::new()),
                map_stride::<V>(),
                map_value_offset::<V>(),
            ),
        ))
    }
}

impl<V, const MAX: usize, const MAX_KEY: usize> Componentize for FrameMap<V, MAX, MAX_KEY> {
    fn sink_columns(&self, _output: &mut impl Decomponentize) {}

    // Trailer budget: `MAX` entries at the entry stride, plus the name pool
    // (`MAX * MAX_KEY`), padded to 8 bytes (frames.md §3.4).
    const MAX_SIZE: usize =
        metor_fsw_ring::round_up8(MAX * map_stride::<V>() as usize + MAX * MAX_KEY);
}

impl<V, const MAX: usize, const MAX_KEY: usize> Decomponentize for FrameMap<V, MAX, MAX_KEY> {
    type Error = core::convert::Infallible;
    fn apply_value(
        &mut self,
        _component_id: ComponentId,
        _view: ComponentView<'_>,
        _timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<V, const MAX: usize, const MAX_KEY: usize> Metadatatize for FrameMap<V, MAX, MAX_KEY> {
    fn metadata(_prefix: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        core::iter::empty()
    }
}
