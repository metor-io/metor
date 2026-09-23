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

/// One filter cycle.
#[system]
fn nav(
    #[state] s: &mut NavState,
    now: Timestamp,
    gps: &mut Input<Gps>,
    gps_backup: &mut Input<Gps>,
    estimate: &mut Output<AttitudeEstimate>,
) {
    if gps.latest().ok().flatten().is_some() || gps_backup.latest().ok().flatten().is_some() {
        s.fixes += 1;
    }
    let _ = estimate.write(&AttitudeEstimate {
        timestamp: now,
        q: [0.0, 0.0, 0.0, 1.0],
    });
}

fn names<P: Ports>(_: &P) -> (&'static str, &'static [&'static str]) {
    (P::NAME, P::NAMES)
}

fn main() {
    assert_eq!(
        names(&nav()),
        ("nav", &["now", "gps", "gps_backup", "estimate"][..])
    );
    let mut table = SystemTable::new();
    table
        .register("nav", nav(), NavState::default)
        .expect("valid records");
    let mut state = NavState::default();
    nav()(
        &mut state,
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
