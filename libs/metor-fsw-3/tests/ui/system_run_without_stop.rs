use metor_fsw_3::{Timestamp, system};

#[system]
async fn idle(now: Timestamp) {
    let _ = now;
}

fn main() {}
