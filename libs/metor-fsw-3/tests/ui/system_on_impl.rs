use metor_fsw_3::{Timestamp, system};

struct Counter(u64);

#[system]
impl Counter {
    fn execute(&mut self, now: Timestamp) {
        self.0 += 1;
        let _ = now;
    }
}

fn main() {}
