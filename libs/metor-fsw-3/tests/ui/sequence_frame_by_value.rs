use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Seq, SeqError, SystemTable, Timestamp, sequence};

#[derive(Clone, Copy, Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Imu {
    #[frame(timestamp)]
    time: Timestamp,
    rate: f64,
}

/// Would see the first `imu` forever: a sequence takes `&Sample<Imu>`.
#[sequence]
async fn watch(seq: &Seq, imu: Option<Imu>) -> Result<(), SeqError> {
    let _ = (seq, imu);
    Ok(())
}

fn main() {
    let mut table = SystemTable::new();
    let _ = table.register_sequence("watch", watch(), || ());
}
