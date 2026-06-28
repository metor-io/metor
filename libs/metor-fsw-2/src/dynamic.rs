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

/// Round `n` up to a multiple of `a` (a power of two).
pub(crate) const fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

/// Entry alignment for a map value type — at least 8 so each entry's `key_off`/
/// `key_len` pair and value stay 8-byte aligned in the trailer.
pub(crate) const fn entry_align<V>() -> usize {
    let a = align_of::<V>();
    if a < 8 { 8 } else { a }
}

/// Byte offset of the value sub-frame within a map entry `{ key_off, key_len, value }`.
pub(crate) const fn map_value_offset<V>() -> u32 {
    align_up(8, entry_align::<V>()) as u32
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
/// Keys must not contain `.` or be empty (rejected loudly at write time).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FrameMap<K, V, const MAX: usize, const MAX_KEY: usize = 32> {
    slot: Slot,
    _kv: PhantomData<(K, V)>,
}

impl<K, V, const MAX: usize, const MAX_KEY: usize> Default for FrameMap<K, V, MAX, MAX_KEY> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<K, V, const MAX: usize, const MAX_KEY: usize> FrameMap<K, V, MAX, MAX_KEY> {
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

impl<K, V: AsVTable, const MAX: usize, const MAX_KEY: usize> AsVTable
    for FrameMap<K, V, MAX, MAX_KEY>
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

impl<K, V, const MAX: usize, const MAX_KEY: usize> Componentize for FrameMap<K, V, MAX, MAX_KEY> {
    fn sink_columns(&self, _output: &mut impl Decomponentize) {}

    // Trailer budget: `MAX` entries at the entry stride, plus the name pool
    // (`MAX * MAX_KEY`), padded to 8 bytes (frames.md §3.4).
    const MAX_SIZE: usize =
        metor_fsw_ring::round_up8(MAX * map_stride::<V>() as usize + MAX * MAX_KEY);
}

impl<K, V, const MAX: usize, const MAX_KEY: usize> Decomponentize for FrameMap<K, V, MAX, MAX_KEY> {
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

impl<K, V, const MAX: usize, const MAX_KEY: usize> Metadatatize for FrameMap<K, V, MAX, MAX_KEY> {
    fn metadata(_prefix: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        core::iter::empty()
    }
}

/// A map key newtype enforcing the dotted-name grammar at construction: no `.`
/// (it would alias the path separator) and non-empty (an empty segment vanishes
/// per the `PathHasher` rule, aliasing `a..b`). The realize path also rejects bad
/// keys with `Error::InvalidComponentData`; this is the loud, write-time guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name<'a>(&'a str);

impl<'a> Name<'a> {
    /// Validates and wraps a key, returning `None` if it is empty or contains `.`.
    pub fn new(s: &'a str) -> Option<Self> {
        if s.is_empty() || s.contains('.') {
            None
        } else {
            Some(Name(s))
        }
    }

    pub fn as_str(&self) -> &'a str {
        self.0
    }
}
