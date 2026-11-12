use metor_fsw_3::{Timestamp, system};

#[system]
fn stamps(now: [Timestamp; 2]) {
    let _ = now;
}

fn main() {}
