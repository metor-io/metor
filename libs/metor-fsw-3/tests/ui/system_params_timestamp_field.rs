use metor_fsw_3::{Log, SystemParams, Timestamp};

#[derive(SystemParams)]
struct Stamped<'a> {
    now: Timestamp,
    log: &'a mut Log,
}

fn main() {}
