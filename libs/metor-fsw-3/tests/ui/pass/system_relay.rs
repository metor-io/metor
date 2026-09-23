use metor_fsw_3::ring::Notifier;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{Frame, Input, Output, Ports, Stop, SystemTable, Timestamp, system};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Gps {
    #[frame(timestamp)]
    timestamp: Timestamp,
    pos: [f64; 3],
}

/// Copies every fix until stop.
#[system]
async fn relay(gps: &mut Input<Gps, Notifier>, out: &mut Output<Gps>, stop: Stop) {
    while !stop.is_set() {
        let Ok(fix) = gps.next().await else { return };
        let fix = Gps {
            timestamp: fix.timestamp,
            pos: fix.pos,
        };
        let _ = out.write(&fix);
    }
}

fn names<P: Ports>(_: &P) -> (&'static str, &'static [&'static str]) {
    (P::NAME, P::NAMES)
}

fn main() {
    assert_eq!(names(&relay()), ("relay", &["gps", "out"][..]));
    let mut table = SystemTable::new();
    table
        .register_async("relay", relay(), Default::default)
        .expect("valid records");
}
