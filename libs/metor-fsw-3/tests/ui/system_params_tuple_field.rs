use metor_fsw_3::{Log, Records, SystemParams};

#[derive(SystemParams)]
struct Pair<'a> {
    pair: (&'a mut Log, &'a Records),
}

fn main() {}
