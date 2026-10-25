//! [`FrameMap`], a frame field holding a runtime-varying set of keyed values.
//!
//! The field itself is an 8-byte [`Slot`] in the frame's fixed region. The
//! entries live in the record's trailer as a fixed-stride array, each entry a
//! `{ key_off: u32, key_len: u32 }` header followed by the value. Offsets are
//! table-absolute. The vtable describes the field with `Op::Map`.

use core::marker::PhantomData;
use core::mem::{align_of, size_of};

use metor_proto::com_de::{Componentize, Decomponentize};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use metor_proto::vtable::builder::{FieldBuilder, map, raw_field};
use metor_proto_wkt::ComponentMetadata;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{AsVTable, Metadatatize, path::ComponentPath};

/// A table-absolute byte offset and byte length into a record's trailer.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes,
)]
pub struct Slot {
    pub off: u32,
    pub byte_len: u32,
}

/// A frame field mapping keys of type `K` to fixed values of type `V`.
///
/// Construct it as [`FrameMap::EMPTY`]. Entries are written by a frame
/// writer and read through a frame view. It is neither `Clone` nor `Copy`,
/// so a slot read from one record cannot be written into another.
#[repr(transparent)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FrameMap<K, V> {
    slot: Slot,
    _kv: PhantomData<fn() -> (K, V)>,
}

impl<K, V> FrameMap<K, V> {
    pub const EMPTY: Self = Self {
        slot: Slot {
            off: 0,
            byte_len: 0,
        },
        _kv: PhantomData,
    };

    /// Alignment of the entry array, at least 8.
    pub const ENTRY_ALIGN: usize = if align_of::<V>() < 8 {
        8
    } else {
        align_of::<V>()
    };
    /// Byte offset of the value within an entry.
    pub const VALUE_OFFSET: usize = size_of::<Slot>().next_multiple_of(Self::ENTRY_ALIGN);
    /// Byte stride of one entry.
    pub const STRIDE: usize =
        (Self::VALUE_OFFSET + size_of::<V>()).next_multiple_of(Self::ENTRY_ALIGN);

    pub fn slot(&self) -> Slot {
        self.slot
    }
}

impl<K, V> Default for FrameMap<K, V> {
    fn default() -> Self {
        Self::EMPTY
    }
}

fn map_fields<K, V: AsVTable>(prefix: &str) -> impl Iterator<Item = FieldBuilder> + use<K, V> {
    const {
        assert!(
            V::ELEMENT,
            "FrameMap value type does not describe its element fields"
        )
    };
    core::iter::once(raw_field(
        0,
        size_of::<Slot>() as u32,
        map(
            prefix,
            V::element_fields(String::new()),
            FrameMap::<K, V>::STRIDE as u32,
            FrameMap::<K, V>::VALUE_OFFSET as u32,
        ),
    ))
}

impl<K, V: AsVTable> AsVTable for FrameMap<K, V> {
    const DYNAMIC: bool = true;
    const ELEMENT: bool = true;

    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        map_fields::<K, V>(&path.to_name())
    }

    fn element_fields(prefix: String) -> impl Iterator<Item = FieldBuilder> {
        map_fields::<K, V>(&prefix)
    }
}

impl<K, V> Componentize for FrameMap<K, V> {
    fn sink_columns(&self, _output: &mut impl Decomponentize) {}

    const MAX_SIZE: usize = 0;
}

impl<K, V> Decomponentize for FrameMap<K, V> {
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

impl<K, V> Metadatatize for FrameMap<K, V> {
    fn metadata(_prefix: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        core::iter::empty()
    }
}

/// Why a key cannot be a path segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    Empty,
    Dot,
    NotUtf8,
}

impl core::fmt::Display for KeyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => f.write_str("map key is empty"),
            Self::Dot => f.write_str("map key contains `.`"),
            Self::NotUtf8 => f.write_str("map key is not UTF-8"),
        }
    }
}

impl std::error::Error for KeyError {}

/// Checks that `bytes` form one dotted-path segment.
pub fn key_str(bytes: &[u8]) -> Result<&str, KeyError> {
    let key = core::str::from_utf8(bytes).map_err(|_| KeyError::NotUtf8)?;
    if key.is_empty() {
        return Err(KeyError::Empty);
    }
    if key.contains('.') {
        return Err(KeyError::Dot);
    }
    Ok(key)
}

/// A type that names [`FrameMap`] entries.
///
/// On the wire every key is a path segment. `Ref` is what a writer takes and
/// a reader yields.
pub trait MapKey {
    type Ref<'a>: Copy + PartialEq;

    /// Writes `key` to the start of `buf`, returning its length, or `None`
    /// if it does not fit.
    fn encode(key: Self::Ref<'_>, buf: &mut [u8]) -> Option<usize>;

    /// Parses a key written by [`encode`](Self::encode).
    fn decode(key: &str) -> Option<Self::Ref<'_>>;

    /// Shortens a key's lifetime, so keys of different lifetimes compare.
    fn shorten<'s, 'l: 's>(key: Self::Ref<'l>) -> Self::Ref<'s>;
}

impl MapKey for String {
    type Ref<'a> = &'a str;

    fn encode(key: &str, buf: &mut [u8]) -> Option<usize> {
        buf.get_mut(..key.len())?.copy_from_slice(key.as_bytes());
        Some(key.len())
    }

    fn decode(key: &str) -> Option<&str> {
        Some(key)
    }

    fn shorten<'s, 'l: 's>(key: &'l str) -> &'s str {
        key
    }
}

/// Writes `n` in decimal to the start of `buf`.
fn encode_decimal(mut n: u64, buf: &mut [u8]) -> Option<usize> {
    let mut digits = [0u8; 20];
    let mut len = 0;
    loop {
        digits[len] = b'0' + (n % 10) as u8;
        len += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let out = buf.get_mut(..len)?;
    for (o, d) in out.iter_mut().zip(digits[..len].iter().rev()) {
        *o = *d;
    }
    Some(len)
}

macro_rules! impl_int_key {
    ($($ty:ty),*) => {$(
        impl MapKey for $ty {
            type Ref<'a> = $ty;

            fn encode(key: $ty, buf: &mut [u8]) -> Option<usize> {
                encode_decimal(u64::from(key), buf)
            }

            fn decode(key: &str) -> Option<$ty> {
                if key.starts_with('+') {
                    return None;
                }
                key.parse().ok()
            }

            fn shorten<'s, 'l: 's>(key: $ty) -> $ty {
                key
            }
        }
    )*};
}

impl_int_key!(u8, u16, u32, u64);

#[cfg(test)]
mod tests {
    use metor_proto::vtable::builder::vtable;

    use super::*;

    #[test]
    fn test_layout_constants() {
        assert_eq!(size_of::<FrameMap<String, u64>>(), 8);
        assert_eq!(FrameMap::<String, u64>::VALUE_OFFSET, 8);
        assert_eq!(FrameMap::<String, u64>::STRIDE, 16);
        assert_eq!(FrameMap::<String, u8>::STRIDE, 16);
        assert_eq!(FrameMap::<String, [u64; 3]>::STRIDE, 32);
    }

    #[test]
    fn test_map_vtable_is_dynamic() {
        let table = vtable(FrameMap::<String, u64>::vtable_fields("nics"));
        assert!(table.is_dynamic());
        const { assert!(<FrameMap<String, u64> as AsVTable>::DYNAMIC) };
        const { assert!(!<u64 as AsVTable>::DYNAMIC) };
    }

    #[test]
    fn test_string_key_round_trip() {
        let mut buf = [0u8; 8];
        assert_eq!(String::encode("en0", &mut buf), Some(3));
        assert_eq!(String::decode("en0"), Some("en0"));
        assert_eq!(String::encode("too_long_key", &mut buf), None);
    }

    #[test]
    fn test_int_key_round_trip() {
        let mut buf = [0u8; 20];
        let len = u32::encode(4211, &mut buf).expect("fits");
        assert_eq!(&buf[..len], b"4211");
        assert_eq!(u32::decode("4211"), Some(4211));
        let len = u64::encode(0, &mut buf).expect("fits");
        assert_eq!(&buf[..len], b"0");
        let len = u64::encode(u64::MAX, &mut buf).expect("fits");
        assert_eq!(
            u64::decode(core::str::from_utf8(&buf[..len]).expect("ascii")),
            Some(u64::MAX)
        );
        assert_eq!(u32::encode(12345, &mut buf[..4]), None);
    }

    #[test]
    fn test_reject_bad_int_keys() {
        assert_eq!(u8::decode("256"), None);
        assert_eq!(u32::decode("+1"), None);
        assert_eq!(u32::decode("-1"), None);
        assert_eq!(u32::decode("en0"), None);
    }

    #[test]
    fn test_key_str() {
        assert_eq!(key_str(b"en0"), Ok("en0"));
        assert_eq!(key_str(b""), Err(KeyError::Empty));
        assert_eq!(key_str(b"a.b"), Err(KeyError::Dot));
        assert_eq!(key_str(&[0xff]), Err(KeyError::NotUtf8));
    }
}
