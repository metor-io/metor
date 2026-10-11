use metor_fsw_3::{Stop, Timestamp, system};

struct Both;

#[system]
impl Both {
    fn execute(&mut self, now: Timestamp) {
        let _ = now;
    }

    async fn run(&mut self, now: Timestamp, stop: Stop) {
        let _ = now;
        stop.wait().await;
    }
}

fn main() {}
