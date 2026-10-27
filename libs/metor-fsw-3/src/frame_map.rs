//! Writing and reading frames whose [`FrameMap`] fields carry a trailer.
//!
//! A record is the frame's fixed bytes followed by a trailer. Each map's
//! entry array is reserved at the trailer tail when the map is started.
//! Keys and nested maps append after it, so every offset is final when it is
//! written.
//!
//! ```text
//! | fixed | pad | entries (reserved) | key | nested map | key | ... |
//! ```
//!
//! A map value must describe its element fields:
//!
//! ```compile_fail
//! use metor_fsw_3::metor_proto::vtable::builder::FieldBuilder;
//! use metor_fsw_3::path::ComponentPath;
//! use metor_fsw_3::{AsVTable, FrameMap};
//!
//! struct Leaf;
//!
//! impl AsVTable for Leaf {
//!     fn vtable_fields(_: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
//!         core::iter::empty()
//!     }
//! }
//!
//! let _ = FrameMap::<u32, Leaf>::as_vtable();
//! ```
//!
//! A frame holding a map must bound its record with `max_len`:
//!
//! ```compile_fail
//! use metor_fsw_3::{Frame, FrameMap, Output, Record, Timestamp};
//! use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//!
//! #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! #[repr(C)]
//! struct Unbounded {
//!     #[frame(timestamp)]
//!     timestamp: Timestamp,
//!     cpu: FrameMap<u32, u64>,
//! }
//! ```

use core::borrow::Borrow;
use core::marker::PhantomData;
use core::mem::size_of;
use core::ops::Deref;

use metor_component::{AsVTable, FrameMap, KeyError, MapKey, Slot, key_str};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::record::DecodeError;

/// An error from building a frame's trailer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameWriteError {
    #[error("record exceeds the frame's {max}-byte bound")]
    Oversize { max: usize },
    #[error("map holds more than the {cap} entries reserved for it")]
    MapFull { cap: usize },
    #[error("map key `{0}` written twice")]
    DuplicateKey(KeyRepr),
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error("map field projection points outside its value")]
    Field,
    #[error("map slot is already set; each map is written once, from `FrameMap::EMPTY`")]
    MapWritten,
}

/// The first bytes of a duplicate key, for error reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRepr {
    bytes: [u8; 16],
    len: u8,
}

impl KeyRepr {
    fn new(key: &[u8]) -> Self {
        let len = key.len().min(16);
        let mut bytes = [0; 16];
        bytes[..len].copy_from_slice(&key[..len]);
        Self {
            bytes,
            len: len as u8,
        }
    }
}

impl core::fmt::Display for KeyRepr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.bytes[..self.len as usize]))
    }
}

/// The record being built: a buffer and the end of what is written.
struct Table<'a> {
    buf: &'a mut [u8],
    len: &'a mut usize,
}

impl Table<'_> {
    fn reborrow(&mut self) -> Table<'_> {
        Table {
            buf: self.buf,
            len: self.len,
        }
    }

    /// Extends the record by `n` zeroed bytes aligned to `align`, returning
    /// where they start.
    fn reserve(&mut self, n: usize, align: usize) -> Result<usize, FrameWriteError> {
        let max = self.buf.len();
        let start = self.len.next_multiple_of(align);
        let end = start
            .checked_add(n)
            .filter(|&end| end <= max)
            .ok_or(FrameWriteError::Oversize { max })?;
        self.buf[*self.len..end].fill(0);
        *self.len = end;
        Ok(start)
    }

    fn write(&mut self, at: usize, bytes: &[u8]) {
        // PANIC Safety: callers write only into the fixed region or a range
        // `reserve` returned, both inside `buf`.
        self.buf[at..at + bytes.len()].copy_from_slice(bytes);
    }

    fn is_zero(&self, at: usize, len: usize) -> bool {
        self.buf
            .get(at..at + len)
            .is_some_and(|bytes| bytes.iter().all(|&b| b == 0))
    }
}

/// Builds the trailer of a value of type `T` placed in a record.
///
/// The root writer covers the frame. [`MapWriter::insert`] returns one for
/// each entry, so nested maps are written the same way.
pub struct FrameWriter<'a, T> {
    table: Table<'a>,
    base: usize,
    value: &'a T,
}

impl<T> FrameWriter<'_, T> {
    /// Writes the map `field` selects, reserving room for `cap` entries.
    pub fn map<K: MapKey, V: AsVTable + IntoBytes + Immutable>(
        &mut self,
        field: impl FnOnce(&T) -> &FrameMap<K, V>,
        cap: usize,
        build: impl FnOnce(&mut MapWriter<'_, K, V>) -> Result<(), FrameWriteError>,
    ) -> Result<(), FrameWriteError> {
        const { assert!(FrameMap::<K, V>::ENTRY_ALIGN <= metor_fsw_3_ring::PAYLOAD_ALIGNMENT) };
        const {
            assert!(
                V::ELEMENT,
                "FrameMap value type does not describe its element fields"
            )
        };
        let slot_at = self.base + field_offset(self.value, field)?;
        if !self.table.is_zero(slot_at, size_of::<Slot>()) {
            return Err(FrameWriteError::MapWritten);
        }
        let bytes = cap
            .checked_mul(FrameMap::<K, V>::STRIDE)
            .ok_or(FrameWriteError::Oversize {
                max: self.table.buf.len(),
            })?;
        let start = self.table.reserve(bytes, FrameMap::<K, V>::ENTRY_ALIGN)?;
        let mut map = MapWriter {
            table: self.table.reborrow(),
            start,
            cap,
            count: 0,
            _kv: PhantomData,
        };
        build(&mut map)?;
        let slot = Slot {
            off: start as u32,
            byte_len: (map.count * FrameMap::<K, V>::STRIDE) as u32,
        };
        map.table.write(slot_at, slot.as_bytes());
        Ok(())
    }
}

/// Returns the offset of the map `field` selects within `value`.
fn field_offset<T, K, V>(
    value: &T,
    field: impl FnOnce(&T) -> &FrameMap<K, V>,
) -> Result<usize, FrameWriteError> {
    let base = core::ptr::from_ref(value).addr();
    let at = core::ptr::from_ref(field(value)).addr();
    at.checked_sub(base)
        .filter(|off| off + size_of::<Slot>() <= size_of::<T>())
        .ok_or(FrameWriteError::Field)
}

/// Appends the entries of one [`FrameMap`].
pub struct MapWriter<'a, K, V> {
    table: Table<'a>,
    start: usize,
    cap: usize,
    count: usize,
    _kv: PhantomData<fn() -> (K, V)>,
}

impl<K: MapKey, V: IntoBytes + Immutable> MapWriter<'_, K, V> {
    /// Adds an entry, returning a writer for the value's own maps.
    ///
    /// The value's `FrameMap` fields should be [`FrameMap::EMPTY`].
    pub fn insert<'w>(
        &'w mut self,
        key: K::Ref<'_>,
        value: &'w V,
    ) -> Result<FrameWriter<'w, V>, FrameWriteError> {
        if self.count == self.cap {
            return Err(FrameWriteError::MapFull { cap: self.cap });
        }
        let (key_off, key_len) = self.write_key(key)?;
        let entry = self.start + self.count * FrameMap::<K, V>::STRIDE;
        let header = Slot {
            off: key_off as u32,
            byte_len: key_len as u32,
        };
        let value_at = entry + FrameMap::<K, V>::VALUE_OFFSET;
        self.table.write(entry, header.as_bytes());
        self.table.write(value_at, value.as_bytes());
        self.count += 1;
        Ok(FrameWriter {
            table: self.table.reborrow(),
            base: value_at,
            value,
        })
    }

    /// Writes `key` at the tail and checks it against earlier keys.
    fn write_key(&mut self, key: K::Ref<'_>) -> Result<(usize, usize), FrameWriteError> {
        let max = self.table.buf.len();
        let at = *self.table.len;
        let oversize = FrameWriteError::Oversize { max };
        let len = K::encode(key, &mut self.table.buf[at..]).ok_or(oversize)?;
        let bytes = at
            .checked_add(len)
            .and_then(|end| self.table.buf.get(at..end))
            .ok_or(oversize)?;
        key_str(bytes)?;
        if self.keys().any(|k| k == bytes) {
            return Err(FrameWriteError::DuplicateKey(KeyRepr::new(bytes)));
        }
        *self.table.len += len;
        Ok((at, len))
    }

    /// The keys written so far.
    fn keys(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.count).map(|i| {
            let entry = self.start + i * FrameMap::<K, V>::STRIDE;
            let (off, len) = read_header(self.table.buf, entry).unwrap_or((0, 0));
            &self.table.buf[off..off + len]
        })
    }
}

/// Starts building a record of `value` into `buf`, returning the record
/// length when `build` succeeds.
pub(crate) fn build_frame<T: IntoBytes + Immutable>(
    value: &T,
    buf: &mut [u8],
    build: impl FnOnce(&mut FrameWriter<'_, T>) -> Result<(), FrameWriteError>,
) -> Result<usize, FrameWriteError> {
    let fixed = value.as_bytes();
    let max = buf.len();
    buf.get_mut(..fixed.len())
        .ok_or(FrameWriteError::Oversize { max })?
        .copy_from_slice(fixed);
    let mut len = fixed.len();
    let mut writer = FrameWriter {
        table: Table { buf, len: &mut len },
        base: 0,
        value,
    };
    build(&mut writer)?;
    Ok(len)
}

/// Reads a `{ off, len }` header at `at`, checked against the table.
fn read_header(table: &[u8], at: usize) -> Option<(usize, usize)> {
    let (slot, _) = Slot::read_from_prefix(table.get(at..)?).ok()?;
    let (off, len) = (slot.off as usize, slot.byte_len as usize);
    table.get(off..off.checked_add(len)?)?;
    Some((off, len))
}

/// A frame borrowed from a record, with its trailer.
pub struct FrameRef<'a, T> {
    table: &'a [u8],
    value: &'a T,
}

impl<T> Clone for FrameRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for FrameRef<'_, T> {}

impl<'a, T: FromBytes + KnownLayout + Immutable> FrameRef<'a, T> {
    /// Borrows the frame at the start of `table`.
    pub fn decode(table: &'a [u8]) -> Result<Self, DecodeError> {
        let value = crate::record::fixed::decode(table)?;
        Ok(Self { table, value })
    }
}

impl<'a, T> FrameRef<'a, T> {
    /// The map `field` selects.
    ///
    /// A slot that does not fit the record reads as an empty map.
    pub fn map<K: MapKey, V>(&self, field: impl FnOnce(&T) -> &FrameMap<K, V>) -> MapRef<'a, K, V> {
        let slot = field(self.value).slot();
        let (off, len) = (slot.off as usize, slot.byte_len as usize);
        let fits = off % FrameMap::<K, V>::ENTRY_ALIGN == 0
            && len % FrameMap::<K, V>::STRIDE == 0
            && off
                .checked_add(len)
                .is_some_and(|end| end <= self.table.len());
        let count = if fits {
            len / FrameMap::<K, V>::STRIDE
        } else {
            0
        };
        MapRef {
            table: self.table,
            start: off,
            count,
            _kv: PhantomData,
        }
    }

    /// The whole record, fixed region first.
    pub fn table(&self) -> &'a [u8] {
        self.table
    }
}

impl<T> Deref for FrameRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> Borrow<T> for FrameRef<'_, T> {
    fn borrow(&self) -> &T {
        self.value
    }
}

/// The entries of one [`FrameMap`] in a record.
pub struct MapRef<'a, K, V> {
    table: &'a [u8],
    start: usize,
    count: usize,
    _kv: PhantomData<fn() -> (K, V)>,
}

impl<'a, K: MapKey, V: FromBytes + KnownLayout + Immutable + 'a> MapRef<'a, K, V> {
    /// The number of entries, including any [`iter`](Self::iter) skips.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The entries in write order. An entry whose key or value does not
    /// read is skipped.
    pub fn iter(&self) -> impl Iterator<Item = (K::Ref<'a>, FrameRef<'a, V>)> + '_ {
        (0..self.count).filter_map(|i| self.entry(i))
    }

    pub fn get(&self, key: K::Ref<'_>) -> Option<FrameRef<'a, V>> {
        self.iter()
            .find(|(k, _)| K::shorten(*k) == K::shorten(key))
            .map(|(_, v)| v)
    }

    fn entry(&self, i: usize) -> Option<(K::Ref<'a>, FrameRef<'a, V>)> {
        let table = self.table;
        let entry = self.start + i * FrameMap::<K, V>::STRIDE;
        let (off, len) = read_header(table, entry)?;
        let key = K::decode(key_str(&table[off..off + len]).ok()?)?;
        let at = entry + FrameMap::<K, V>::VALUE_OFFSET;
        let value = V::ref_from_bytes(table.get(at..at + size_of::<V>())?).ok()?;
        Some((key, FrameRef { table, value }))
    }
}

#[cfg(test)]
mod tests {
    use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
    use metor_proto::types::{ComponentId, Timestamp};
    use metor_proto::vtable::builder::vtable;

    use super::*;
    use crate::{Frame, Payload, Record};

    #[derive(
        AsVTable,
        Metadatatize,
        Componentize,
        Decomponentize,
        IntoBytes,
        Immutable,
        KnownLayout,
        FromBytes,
        Debug,
    )]
    #[repr(C)]
    struct Nic {
        rx_bytes: u64,
        ethtool: FrameMap<String, u64>,
    }

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[frame(name = "nics", max_len = 512)]
    #[repr(C)]
    struct Nics {
        #[frame(timestamp)]
        timestamp: Timestamp,
        nics: FrameMap<String, Nic>,
        procs: FrameMap<u32, u64>,
    }

    const FIXED: Nics = Nics {
        timestamp: Timestamp(7),
        nics: FrameMap::EMPTY,
        procs: FrameMap::EMPTY,
    };

    const NIC: Nic = Nic {
        rx_bytes: 0,
        ethtool: FrameMap::EMPTY,
    };

    fn nic(rx_bytes: u64) -> Nic {
        Nic { rx_bytes, ..NIC }
    }

    /// Builds a record into a 16-aligned buffer, as a ring payload is.
    fn build(
        buf: &mut Aligned,
        f: impl FnOnce(&mut FrameWriter<'_, Nics>) -> Result<(), FrameWriteError>,
    ) -> Result<usize, FrameWriteError> {
        build_frame(&FIXED, &mut buf.0, f)
    }

    #[repr(C, align(16))]
    struct Aligned([u8; 512]);

    fn write_two_nics(w: &mut FrameWriter<'_, Nics>) -> Result<(), FrameWriteError> {
        let (en0, en1) = (nic(10), nic(20));
        w.map(
            |f| &f.nics,
            2,
            |m| {
                m.insert("en0", &en0)?.map(
                    |n| &n.ethtool,
                    2,
                    |e| {
                        e.insert("rx_crc", &3)?;
                        e.insert("tx_drop", &4)?;
                        Ok(())
                    },
                )?;
                m.insert("en1", &en1)?;
                Ok(())
            },
        )?;
        w.map(
            |f| &f.procs,
            4,
            |m| {
                m.insert(4211, &99)?;
                Ok(())
            },
        )
    }

    #[test]
    fn test_nested_round_trip() {
        let mut buf = Aligned([0xAA; 512]);
        let len = build(&mut buf, write_two_nics).expect("fits");
        let frame = Nics::decode(&buf.0[..len]).expect("decodes");
        assert_eq!(frame.timestamp, Timestamp(7));
        let nics = frame.map(|f| &f.nics);
        assert_eq!(nics.len(), 2);
        let en0 = nics.get("en0").expect("en0");
        assert_eq!(en0.rx_bytes, 10);
        let ethtool: Vec<_> = en0
            .map(|n| &n.ethtool)
            .iter()
            .map(|(k, v)| (k, *v))
            .collect();
        assert_eq!(ethtool, vec![("rx_crc", 3), ("tx_drop", 4)]);
        assert!(nics.get("en1").expect("en1").map(|n| &n.ethtool).is_empty());
        assert_eq!(nics.get(&String::from("en9")).map(|n| n.rx_bytes), None);
        assert_eq!(frame.map(|f| &f.procs).get(4211).map(|v| *v), Some(99));
    }

    #[test]
    fn test_vtable_walk_matches_reader() {
        let mut buf = Aligned([0; 512]);
        let len = build(&mut buf, write_two_nics).expect("fits");
        let table = vtable(Nics::vtable_fields("host.nics".to_string()));
        let mut seen = Vec::new();
        table
            .for_each_field::<metor_proto::error::Error>(Some(&buf.0[..len]), &mut |rf| {
                let view = rf.view.expect("value");
                let bytes = view.as_bytes();
                seen.push((rf.component_id, u64::read_from_bytes(bytes).ok()));
                Ok(())
            })
            .expect("walks");
        let id = |s: &str| ComponentId::new(s);
        assert_eq!(
            seen,
            vec![
                (id("host.nics.nics.en0.rx_bytes"), Some(10)),
                (id("host.nics.nics.en0.ethtool.rx_crc"), Some(3)),
                (id("host.nics.nics.en0.ethtool.tx_drop"), Some(4)),
                (id("host.nics.nics.en1.rx_bytes"), Some(20)),
                (id("host.nics.procs.4211"), Some(99)),
            ]
        );
    }

    #[test]
    fn test_unused_reservation_is_skipped() {
        let mut buf = Aligned([0xAA; 512]);
        let len = build(&mut buf, |w| {
            w.map(
                |f| &f.procs,
                8,
                |m| {
                    m.insert(1, &5)?;
                    Ok(())
                },
            )
        })
        .expect("fits");
        let frame = Nics::decode(&buf.0[..len]).expect("decodes");
        assert_eq!(frame.map(|f| &f.procs).len(), 1);
        assert!(frame.map(|f| &f.nics).is_empty());
    }

    #[test]
    fn test_reject_bad_writes() {
        let mut buf = Aligned([0; 512]);
        let full = build(&mut buf, |w| {
            w.map(
                |f| &f.procs,
                1,
                |m| {
                    m.insert(1, &1)?;
                    m.insert(2, &2)?;
                    Ok(())
                },
            )
        });
        assert_eq!(full, Err(FrameWriteError::MapFull { cap: 1 }));
        let dup = build(&mut buf, |w| {
            w.map(
                |f| &f.procs,
                2,
                |m| {
                    m.insert(1, &1)?;
                    m.insert(1, &2)?;
                    Ok(())
                },
            )
        });
        assert!(matches!(dup, Err(FrameWriteError::DuplicateKey(_))));
        let dot = build(&mut buf, |w| {
            w.map(|f| &f.nics, 1, |m| m.insert("a.b", &NIC).map(drop))
        });
        assert_eq!(dot, Err(FrameWriteError::Key(KeyError::Dot)));
        let empty = build(&mut buf, |w| {
            w.map(|f| &f.nics, 1, |m| m.insert("", &NIC).map(drop))
        });
        assert_eq!(empty, Err(FrameWriteError::Key(KeyError::Empty)));
        let oversize = build(&mut buf, |w| w.map(|f| &f.procs, 100, |_| Ok(())));
        assert_eq!(oversize, Err(FrameWriteError::Oversize { max: 512 }));
        static OTHER: FrameMap<u32, u64> = FrameMap::EMPTY;
        let outside = build(&mut buf, |w| w.map(|_| &OTHER, 1, |_| Ok(())));
        assert_eq!(outside, Err(FrameWriteError::Field));
    }

    #[test]
    fn test_corrupt_slot_reads_empty() {
        let mut buf = Aligned([0; 512]);
        let len = build(&mut buf, write_two_nics).expect("fits");
        let at = core::mem::offset_of!(Nics, nics);
        buf.0[at..at + 4].copy_from_slice(&10_000u32.to_le_bytes());
        let frame = Nics::decode(&buf.0[..len]).expect("decodes");
        assert!(frame.map(|f| &f.nics).is_empty());
    }

    #[test]
    fn test_corrupt_key_skips_entry() {
        let mut buf = Aligned([0; 512]);
        let len = build(&mut buf, |w| {
            w.map(
                |f| &f.procs,
                2,
                |m| {
                    m.insert(1, &1)?;
                    m.insert(2, &2)?;
                    Ok(())
                },
            )
        })
        .expect("fits");
        let frame = Nics::decode(&buf.0[..len]).expect("decodes");
        let start = frame.procs.slot().off as usize;
        buf.0[start + 4..start + 8].copy_from_slice(&1000u32.to_le_bytes());
        let frame = Nics::decode(&buf.0[..len]).expect("decodes");
        let keys: Vec<_> = frame.map(|f| &f.procs).iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![2]);
    }

    #[test]
    fn test_reject_map_written_twice() {
        let mut buf = Aligned([0; 512]);
        let twice = build(&mut buf, |w| {
            w.map(|f| &f.procs, 1, |_| Ok(()))?;
            w.map(|f| &f.procs, 1, |_| Ok(()))
        });
        assert_eq!(twice, Err(FrameWriteError::MapWritten));
    }

    /// A padding type with no component traits.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    struct Opaque([u8; 8]);

    #[derive(
        AsVTable,
        Metadatatize,
        Componentize,
        Decomponentize,
        IntoBytes,
        Immutable,
        KnownLayout,
        FromBytes,
    )]
    #[repr(C, align(16))]
    struct Wide {
        a: [u64; 3],
        #[fsw(skip)]
        _pad: Opaque,
    }

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[frame(name = "wides", max_len = 256)]
    #[repr(C)]
    struct Wides {
        #[frame(timestamp)]
        timestamp: Timestamp,
        _pad: u64,
        wide: FrameMap<u8, Wide>,
    }

    #[test]
    fn test_aligned_array_value_round_trip() {
        assert_eq!(FrameMap::<u8, Wide>::VALUE_OFFSET, 16);
        let fixed = Wides {
            timestamp: Timestamp(1),
            _pad: 0,
            wide: FrameMap::EMPTY,
        };
        let wide = Wide {
            a: [1, 2, 3],
            _pad: Opaque([0; 8]),
        };
        let mut buf = Aligned([0; 512]);
        let len = build_frame(&fixed, &mut buf.0[..256], |w| {
            w.map(|f| &f.wide, 1, |m| m.insert(5, &wide).map(drop))
        })
        .expect("fits");
        let frame = Wides::decode(&buf.0[..len]).expect("decodes");
        assert_eq!(frame.map(|f| &f.wide).get(5).map(|v| v.a), Some([1, 2, 3]));
        let table = vtable(Wides::vtable_fields("w".to_string()));
        let mut seen = Vec::new();
        table
            .for_each_field::<metor_proto::error::Error>(Some(&buf.0[..len]), &mut |rf| {
                seen.push(rf.component_id);
                Ok(())
            })
            .expect("walks");
        let id = |s: &str| ComponentId::new(s);
        assert_eq!(
            seen,
            vec![id("w.wide.5.a.0"), id("w.wide.5.a.1"), id("w.wide.5.a.2")]
        );
    }

    #[test]
    fn test_max_len_sets_record_bound() {
        assert_eq!(Nics::MAX_LEN, 512);
        const { assert!(<Nics as AsVTable>::DYNAMIC) };
    }
}
