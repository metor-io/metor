use metor_fsw_3::{Stop, system};

struct Idle;

#[system]
impl Idle {
    fn run(&mut self, stop: Stop) {
        let _ = stop;
    }
}

fn main() {}
