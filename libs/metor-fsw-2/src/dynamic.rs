//! Runtime-sized frame members, [`FrameList`] and [`FrameMap`].
//!
//! A `#[repr(C)]` frame stays fixed-size by representing each dynamic member
//! as an 8-byte [`Slot`] holding a table-absolute offset and a byte length.
//! The element data itself lives in the table trailer, past the fixed region,
//! where [`FrameWriter`](crate::writer::FrameWriter) appends it and patches
//! the slot. A list block is the element frames laid back to back. A map
//! block is a fixed-stride entry array, each entry a `MapEntryHeader`
//! followed by the value, trailed by a name pool the key offsets point into.
//!
//! The const generics bound the trailer. `MAX` caps the element or entry
//! count and `MAX_KEY` the per-key byte length, which keeps
//! [`Componentize::MAX_SIZE`] a `const` expression so output rings can be
//! sized at construction. A list budgets `MAX` elements at the element
//! stride, and a map budgets `MAX` entries at the entry stride plus a
//! `MAX * MAX_KEY` name pool, each rounded up to a multiple of 8.
//!
//! Both types name their elements by dotted path, and the vtable name they
//! emit depends on how they are reached. A field reached statically uses the
//! full dotted prefix from the frame root, while a field reached as the
//! element type of an enclosing list or map uses only its own relative name;
//! the enclosing op accumulates the runtime part of the path.
//!
//! Element names cannot be listed at compile time, so neither type emits
//! static metadata for each index or key. The vtable holds the element form,
//! and each record supplies the live indices or keys.

use core::marker::PhantomData;
use core::mem::{align_of, size_of};

use metor_fsw::path::ComponentPath;
use metor_fsw::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use metor_proto::vtable::builder::{FieldBuilder, list, map, raw_field};
use metor_proto_wkt::ComponentMetadata;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// An 8-byte reference from a frame's fixed region to a dynamic member's
/// data in the trailer, as a table-absolute byte offset plus a byte length.
///
/// [`FrameList`] and [`FrameMap`] are each transparent wrappers over one of
/// these; [`FrameWriter`](crate::writer::FrameWriter) fills the trailer and
/// patches the slot to point at it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Slot {
    pub trailer_off: u32,
    pub byte_len: u32,
}

/// Every map entry begins with this pair, which locates the entry's key in
/// the name pool by table-absolute offset and byte length.
///
/// Stored native-endian like every other frame field. In practice the format
/// is little-endian, since all supported targets are LE and the ring's
/// `arch_tag` rejects cross-endian producers.
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

/// Pack a string into a fixed `N`-byte buffer, truncating, and return the
/// used length.
pub(crate) fn pack_str<const N: usize>(s: &str) -> ([u8; N], u8) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(N);
    let mut buf = [0u8; N];
    buf[..n].copy_from_slice(&bytes[..n]);
    (buf, n as u8)
}

/// Alignment of a map entry for value type `V`, at least 8 so the
/// `MapEntryHeader` and value stay 8-byte aligned in the trailer.
pub(crate) const fn entry_align<V>() -> usize {
    let a = align_of::<V>();
    if a < 8 { 8 } else { a }
}

/// Byte offset of the value within a map entry, after the header.
pub(crate) const fn map_value_offset<V>() -> u32 {
    align_up(size_of::<MapEntryHeader>(), entry_align::<V>()) as u32
}

/// Byte stride of one map entry in the entry array.
pub(crate) const fn map_stride<V>() -> u32 {
    align_up(
        map_value_offset::<V>() as usize + size_of::<V>(),
        entry_align::<V>(),
    ) as u32
}

/// A frame field holding a runtime-varying number of element frames,
/// addressed by position (`processes.0.pid`). `MAX` bounds the element count
/// for buffer sizing.
///
/// `#[repr(transparent)]` over its [`Slot`] (the `PhantomData` is a ZST), so
/// the field is exactly the 8-byte slot and stays trivially `IntoBytes`. The
/// elements themselves live in the trailer; see the module doc for the
/// layout.
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
    /// An empty, zeroed slot. Construct frames with this and let the
    /// [`FrameWriter`](crate::writer::FrameWriter) patch it.
    pub const EMPTY: Self = Self {
        slot: Slot {
            trailer_off: 0,
            byte_len: 0,
        },
        _ty: PhantomData,
    };

    /// The raw slot.
    pub const fn slot(&self) -> Slot {
        self.slot
    }
}

impl<T: AsVTable, const MAX: usize> AsVTable for FrameList<T, MAX> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        // Reached statically, so the list op takes the full dotted prefix (the
        // naming rule in the module doc); as a member template it is the
        // relative field name only. The slot sits at offset 0; the enclosing
        // frame `offset_by`s it into place.
        Self::element_fields(path.to_name())
    }

    fn element_fields(prefix: String) -> impl Iterator<Item = FieldBuilder> {
        core::iter::once(raw_field(
            0,
            size_of::<Slot>() as u32,
            list(
                &prefix,
                T::element_fields(String::new()),
                size_of::<T>() as u32,
            ),
        ))
    }
}

impl<T, const MAX: usize> Componentize for FrameList<T, MAX> {
    // The slot itself carries no value; elements flow through the vtable and
    // trailer.
    fn sink_columns(&self, _output: &mut impl Decomponentize) {}

    // MAX elements at the element stride, padded to 8.
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
        core::iter::empty()
    }
}

/// A frame field holding a runtime-varying number of element frames,
/// addressed by string key (`processes.htop.pid`). `MAX` bounds the entry
/// count and `MAX_KEY` the per-key byte length for buffer sizing.
///
/// Keys are plain `&str`s; [`MapWriter::insert`](crate::MapWriter) rejects
/// empty keys and keys containing `.` at write time.
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
    /// An empty, zeroed slot.
    pub const EMPTY: Self = Self {
        slot: Slot {
            trailer_off: 0,
            byte_len: 0,
        },
        _kv: PhantomData,
    };

    /// The raw slot.
    pub const fn slot(&self) -> Slot {
        self.slot
    }
}

impl<V: AsVTable, const MAX: usize, const MAX_KEY: usize> AsVTable for FrameMap<V, MAX, MAX_KEY> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        Self::element_fields(path.to_name())
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

    // MAX entries at the entry stride plus the name pool, padded to 8.
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
