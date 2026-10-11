use metor_fsw_3::{Timestamp, system};

struct Idle;

#[system]
impl Idle {
    async fn run(&mut self, now: Timestamp) {
        let _ = now;
    }
}

fn main() {}
