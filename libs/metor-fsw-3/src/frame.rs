//! The [`Frame`] trait, implemented via `#[derive(Frame)]`.

use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_proto::types::{ComponentId, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// A fixed `#[repr(C)]` struct whose fields share one timestamp and whose
/// bytes are the ring payload.
pub trait Frame:
    AsVTable
    + Componentize
    + Decomponentize
    + Metadatatize
    + IntoBytes
    + FromBytes
    + KnownLayout
    + Immutable
{
    /// Dotted prefix of every member component's path.
    const NAME: &'static str;
    /// Identifier hashed from [`NAME`](Frame::NAME).
    const ID: ComponentId = ComponentId::new(Self::NAME);

    /// The shared timestamp, read from the `#[frame(timestamp)]` field.
    fn timestamp(&self) -> Timestamp;
}

#[cfg(test)]
mod tests {
    use metor_proto::types::{ComponentId, Timestamp};
    use zerocopy::{FromBytes, IntoBytes};

    use crate::tests::utils::{BareName, Imu};
    use crate::{Componentize, Frame};

    #[test]
    fn derive_sets_name_id_and_timestamp() {
        let imu = Imu::new(42, 1.0);
        assert_eq!(Imu::NAME, "imu");
        assert_eq!(Imu::ID, ComponentId::new("imu"));
        assert_eq!(imu.timestamp(), Timestamp(42));
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
