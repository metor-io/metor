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

struct Relay;

#[system]
impl Relay {
    /// Copies every fix until stop.
    async fn run(&mut self, gps: &mut Input<Gps, Notifier>, out: &mut Output<Gps>, stop: Stop) {
        while !stop.is_set() {
            let Ok(fix) = gps.next().await else { return };
            let fix = Gps {
                timestamp: fix.timestamp,
                pos: fix.pos,
            };
            let _ = out.write(&fix);
        }
    }
}

fn main() {
    assert_eq!(Relay::NAMES, &["gps", "out"]);
    assert_eq!(Relay::NAME, "relay");
    let mut table = SystemTable::new();
    table
        .register_async("relay", || Relay)
        .expect("valid records");
}
