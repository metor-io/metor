use metor_fsw_3::{Timestamp, system};

struct Counter(u32);

#[system]
impl Counter {
    fn execute(&mut self, now: Timestamp, step: u32) {
        self.0 += step;
        let _ = now;
    }
}

fn main() {}
