//! The [`Frame`] trait, implemented via `#[derive(Frame)]`.

use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Record;

/// A `Frame` is a fixed `#[repr(C)]` that can be decomposed into components
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
