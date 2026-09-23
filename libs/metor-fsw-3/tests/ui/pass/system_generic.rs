use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Input, Ports, SystemTable, Timestamp, system};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Imu {
    #[frame(timestamp)]
    timestamp: Timestamp,
    omega: [f64; 3],
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Gps {
    #[frame(timestamp)]
    timestamp: Timestamp,
    pos: [f64; 3],
}

#[derive(Default)]
struct Seen(u64);

#[system]
fn probe<T: Frame>(#[state] seen: &mut Seen, input: &mut Input<T>) {
    if input.latest().ok().flatten().is_some() {
        seen.0 += 1;
    }
}

fn names<P: Ports>(_: &P) -> (&'static str, &'static [&'static str]) {
    (P::NAME, P::NAMES)
}

fn main() {
    assert_eq!(names(&probe::<Imu>()), ("probe", &["input"][..]));
    let mut table = SystemTable::new();
    table
        .register("imu_probe", probe::<Imu>(), Seen::default)
        .expect("valid records");
    table
        .register("gps_probe", probe::<Gps>(), Seen::default)
        .expect("valid records");
    let mut seen = Seen::default();
    probe::<Imu>()(&mut seen, &mut Input::try_new(Vec::new()).unwrap());
}
