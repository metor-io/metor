use metor_fsw_3::{Log, SystemParams};

#[derive(SystemParams)]
struct Flat<'a> {
    #[fsw(flatten)]
    log: &'a mut Log,
}

fn main() {}
