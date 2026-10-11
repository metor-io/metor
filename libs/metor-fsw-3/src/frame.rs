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

    use crate::tests::utils::{BareName, Imu};
    use crate::{Componentize, Record};

    #[test]
    fn derive_sets_name_id_and_timestamp() {
        let imu = Imu::new(42, 1.0);
        assert_eq!(Imu::NAME, "imu");
        assert_eq!(Imu::ID, ComponentId::new("imu"));
        assert_eq!(imu.timestamp(), Some(Timestamp(42)));
        assert!(Imu::MAX_SIZE >= size_of::<Imu>());
    }

    #[test]
    fn name_defaults_to_snake_case_ident() {
        assert_eq!(BareName::NAME, "bare_name");
    }

    #[test]
    fn bytes_round_trip() {
        let imu = Imu::new(7, 9.8);
        let back = Imu::read_from_bytes(imu.as_bytes()).expect("exact size");
        assert_eq!(back, imu);
        assert!(Imu::read_from_bytes(&imu.as_bytes()[..8]).is_err());
    }
}
