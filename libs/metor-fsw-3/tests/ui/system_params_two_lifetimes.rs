use metor_fsw_3::{Log, SystemParams, Timestamp};

#[derive(SystemParams)]
struct Two<'a, 'b> {
    log: &'a mut Log,
    now: &'b Timestamp,
}

fn main() {}
