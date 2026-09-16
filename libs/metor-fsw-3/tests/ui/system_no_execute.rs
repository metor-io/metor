use metor_fsw_3::system;

struct Idle;

#[system]
impl Idle {
    fn step(&mut self) {}
}

fn main() {}
