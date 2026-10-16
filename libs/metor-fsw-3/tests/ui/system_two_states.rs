use metor_fsw_3::{Timestamp, system};

struct Counter(u64);

#[system]
fn counter(#[state] a: &mut Counter, #[state] b: &mut Counter, now: Timestamp) {
    a.0 += b.0;
    let _ = now;
}

fn main() {}
