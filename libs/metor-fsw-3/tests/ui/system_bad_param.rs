use metor_fsw_3::{Timestamp, system};

struct Counter(u32);

#[system]
fn counter(#[state] s: &mut Counter, now: Timestamp, step: u32) {
    s.0 += step;
    let _ = now;
}

fn main() {}
