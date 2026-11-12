use metor_fsw_3::{Abort, SystemParams};

#[derive(SystemParams)]
struct Aborts<'a> {
    aborts: [&'a mut Abort; 2],
}

fn main() {}
