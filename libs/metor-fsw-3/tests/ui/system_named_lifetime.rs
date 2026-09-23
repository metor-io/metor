use metor_fsw_3::{Timestamp, system};

#[system]
fn stamp(now: Timestamp, name: &'static str) {
    let _ = (now, name);
}

fn main() {}
