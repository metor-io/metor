use metor_fsw_3::{Timestamp, system};

struct Counter(u64);

#[system]
fn counter(#[state] s: Counter, now: Timestamp) {
    let _ = (s.0, now);
}

fn main() {}
