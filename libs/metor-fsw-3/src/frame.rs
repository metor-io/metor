//! The [`Frame`] trait, implemented with `#[derive(Frame)]`.

use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Record;

/// A fixed-layout record that is stored as its raw bytes.
///
/// Frames are `#[repr(C)]` structs. Readers borrow them straight from the ring
/// without decoding. Each field is also a telemetry component, so the ground
/// can plot it.
pub trait Frame:
    Record
    + AsVTable
    + Componentize
    + Decomponentize
    + Metadatatize
    + IntoBytes
    + FromBytes
    + KnownLayout
    + Immutable
{
}

#[cfg(test)]
mod tests {
    use crate::Record;
    use crate::tests::utils::BareName;

    #[test]
    fn test_default_frame_name() {
        assert_eq!(BareName::NAME, "bare_name");
    }
}
