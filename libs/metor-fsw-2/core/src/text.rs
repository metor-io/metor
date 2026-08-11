//! [`FrameStr`], a fixed-capacity UTF-8 string frame field.
//!
//! A `#[repr(C)]` frame stays fixed-size, so a string member is a `CAP`-byte
//! buffer NUL-padded to its used length — length is implicit in the first NUL,
//! never a separate field. It telemeters as a single shaped `U8 × [CAP]`
//! component tagged `is_string`, which the panel renders as text; the older
//! `[u8; CAP]` spelling instead fanned out into `CAP` per-byte components
//! through the `[T; N]` blanket [`AsVTable`].
//!
//! Truly unbounded text belongs in messages
//! ([`LogEvent`](crate::health::LogEvent)-style), not in a cyclic frame.

use core::mem::size_of;

use metor_component::path::ComponentPath;
use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_proto::types::{ComponentId, ComponentView, PrimType, Timestamp};
use metor_proto::vtable::builder::{self, FieldBuilder, raw_field, schema};
use metor_proto_wkt::ComponentMetadata;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// A fixed-capacity UTF-8 string frame field: a `CAP`-byte buffer whose used
/// length is the run before the first NUL. Shrinking is lossless because
/// [`set`](FrameStr::set) zeroes the tail, and reads stop at the first NUL, so
/// a stale suffix never leaks.
///
/// `#[repr(transparent)]` over the buffer keeps the field trivially
/// `IntoBytes`/`FromBytes`, so it composes into a zerocopy frame like any
/// other fixed member.
#[repr(transparent)]
#[derive(Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct FrameStr<const CAP: usize> {
    buf: [u8; CAP],
}

impl<const CAP: usize> Default for FrameStr<CAP> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<const CAP: usize> FrameStr<CAP> {
    /// A zeroed, empty string.
    pub const EMPTY: Self = Self { buf: [0; CAP] };

    /// A string holding `s`, truncated on a char boundary to fit `CAP`.
    pub fn new(s: &str) -> Self {
        let mut this = Self::EMPTY;
        this.set(s);
        this
    }

    /// Overwrite with `s`, truncating on a char boundary to fit `CAP` and
    /// zeroing the unused tail so no earlier content survives.
    pub fn set(&mut self, s: &str) {
        let mut n = s.len().min(CAP);
        while n > 0 && !s.is_char_boundary(n) {
            n -= 1;
        }
        self.buf = [0; CAP];
        self.buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    }

    /// The stored text, up to the first NUL. Invalid UTF-8 decodes to `""`
    /// rather than panicking, since the buffer is raw telemetry bytes.
    pub fn as_str(&self) -> &str {
        let end = self.buf.iter().position(|&b| b == 0).unwrap_or(CAP);
        core::str::from_utf8(&self.buf[..end]).unwrap_or("")
    }

    /// Whether the string is empty (first byte is NUL).
    pub fn is_empty(&self) -> bool {
        self.buf.first() == Some(&0)
    }
}

impl<const CAP: usize> From<&str> for FrameStr<CAP> {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl<const CAP: usize> core::fmt::Debug for FrameStr<CAP> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const CAP: usize> AsVTable for FrameStr<CAP> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = if path.is_empty() {
            builder::component("str")
        } else {
            builder::component(path.to_component_id())
        };
        core::iter::once(raw_field(
            0,
            CAP as u32,
            schema(PrimType::U8, &[CAP as u64], component),
        ))
    }

    // As a dynamic element member the buffer is a `path_component` leaf named by
    // its relative path; the enclosing `list`/`map` composes the runtime prefix.
    fn element_fields(prefix: String) -> impl Iterator<Item = FieldBuilder> {
        core::iter::once(raw_field(
            0,
            CAP as u32,
            schema(PrimType::U8, &[CAP as u64], builder::path_component(&prefix)),
        ))
    }
}

impl<const CAP: usize> Metadatatize for FrameStr<CAP> {
    fn metadata(prefix: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        core::iter::once(prefix.to_metadata().with_string())
    }
}

impl<const CAP: usize> Componentize for FrameStr<CAP> {
    // The buffer flows through the vtable as one shaped component; there is no
    // separate columnar value to sink.
    fn sink_columns(&self, _output: &mut impl Decomponentize) {}

    const MAX_SIZE: usize = size_of::<Self>();
}

impl<const CAP: usize> Decomponentize for FrameStr<CAP> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Frame;
    use metor_proto::types::Timestamp;

    #[test]
    fn set_truncates_on_char_boundary() {
        // "é" is two bytes; a 2-cap buffer can hold 'a' but not the split 'é',
        // so it must drop the whole char rather than leave a half.
        let mut s = FrameStr::<2>::EMPTY;
        s.set("aé");
        assert_eq!(s.as_str(), "a");
    }

    #[test]
    fn set_zeroes_the_tail() {
        let mut s = FrameStr::<8>::new("commissioning");
        assert_eq!(s.as_str(), "commissi");
        s.set("go");
        assert_eq!(s.as_str(), "go");
        // The 'm','m','i'… from the longer prior value are gone, not just hidden.
        assert_eq!(s.is_empty(), false);
        assert_eq!(&s.buf[2..], &[0u8; 6]);
    }

    #[test]
    fn roundtrip_and_empty() {
        assert_eq!(FrameStr::<16>::EMPTY.as_str(), "");
        assert!(FrameStr::<16>::EMPTY.is_empty());
        assert_eq!(FrameStr::<16>::new("hello").as_str(), "hello");
    }

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    #[metor_fsw(name = "sample")]
    struct Sample {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        #[metor_fsw(nest)]
        label: FrameStr<8>,
    }

    #[test]
    fn frame_emits_one_string_component() {
        use zerocopy::IntoBytes;
        let sample = Sample {
            timestamp: Timestamp(1),
            label: FrameStr::new("go"),
        };
        let vtable = <Sample as AsVTable>::as_vtable();
        let fields: Vec<_> = vtable
            .realize_fields(Some(sample.as_bytes()))
            .map(|f| f.unwrap())
            .collect();
        assert_eq!(fields.len(), 1, "one component for the whole buffer");
        assert_eq!(fields[0].component_id, ComponentId::new("sample.label"));
        assert_eq!(fields[0].ty, PrimType::U8);
        assert_eq!(fields[0].shape, &[8_usize] as &[usize]);

        let meta = <Sample as Metadatatize>::metadata(()).collect::<Vec<_>>();
        let label = meta
            .iter()
            .find(|m| m.name == "sample.label")
            .expect("label metadata");
        assert!(label.is_string(), "label carries the is_string flag");
    }
}
