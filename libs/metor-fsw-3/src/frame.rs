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
    use metor_proto::types::{ComponentId, Timestamp};
    use zerocopy::{FromBytes, IntoBytes};

    use crate::Record;
    use crate::tests::utils::{BareName, Imu};

    #[test]
    fn test_derived_frame_metadata() {
        let imu = Imu::new(42, 1.0);
        assert_eq!(Imu::NAME, "imu");
        assert_eq!(Imu::ID, ComponentId::new("imu"));
        assert_eq!(imu.timestamp(), Some(Timestamp(42)));
    }

    #[test]
    fn test_default_frame_name() {
        assert_eq!(BareName::NAME, "bare_name");
    }

    #[test]
    fn test_frame_bytes_roundtrip() {
        let imu = Imu::new(7, 9.8);
        let back = Imu::read_from_bytes(imu.as_bytes()).expect("exact size");
        assert_eq!(back, imu);
        assert!(Imu::read_from_bytes(&imu.as_bytes()[..8]).is_err());
    }
}
