use metor_fsw_3::{Log, Seq, SystemParams};

#[derive(SystemParams)]
struct Armed<'a> {
    seq: &'a Seq<'a>,
    log: &'a mut Log,
}

fn main() {}
