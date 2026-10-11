use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Input, Output, Ports, SystemTable, Timestamp, system};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Gps {
    #[frame(timestamp)]
    timestamp: Timestamp,
    pos: [f64; 3],
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct AttitudeEstimate {
    #[frame(timestamp)]
    timestamp: Timestamp,
    q: [f64; 4],
}

#[derive(Default)]
struct NavState {
    fixes: u64,
}

#[system]
impl NavState {
    /// One filter cycle.
    fn execute(
        &mut self,
        now: Timestamp,
        gps: &mut Input<Gps>,
        gps_backup: &mut Input<Gps>,
        estimate: &mut Output<AttitudeEstimate>,
    ) {
        if gps.latest().ok().flatten().is_some() || gps_backup.latest().ok().flatten().is_some() {
            self.fixes += 1;
        }
        let _ = estimate.write(&AttitudeEstimate {
            timestamp: now,
            q: [0.0, 0.0, 0.0, 1.0],
        });
    }
}

fn main() {
    assert_eq!(NavState::NAMES, &["now", "gps", "gps_backup", "estimate"]);
    assert_eq!(NavState::NAME, "nav_state");
    let mut table = SystemTable::new();
    table
        .register("nav", NavState::default)
        .expect("valid records");
    let mut nav = NavState::default();
    nav.execute(
        Timestamp(0),
        &mut Input::try_new(Vec::new()).unwrap(),
        &mut Input::try_new(Vec::new()).unwrap(),
        &mut Output::try_new(
            metor_fsw_3::ring::RingBuffer::create_in_memory(metor_fsw_3::ring::Config {
                capacity: 256,
                max_readers: 1,
            })
            .writer(metor_fsw_3::ring::NoWake)
            .unwrap(),
        )
        .unwrap(),
    );
}
