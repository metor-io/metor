//! The [`Frame`] trait, implemented with `#[derive(Frame)]`.

use metor_component::{AsVTable, Metadatatize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Record;

/// A fixed-layout record that is stored as its raw bytes.
///
/// Frames are `#[repr(C)]` structs. Readers borrow them straight from the ring
/// without decoding. Each field is also a telemetry component, so the ground
/// can plot it.
pub trait Frame:
    Record + AsVTable + Metadatatize + IntoBytes + FromBytes + KnownLayout + Immutable
{
    /// The offset and size of the field that holds the frame's timestamp.
    const STAMP: (u32, u32);
}

#[cfg(test)]
mod tests;
