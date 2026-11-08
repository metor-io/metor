use metor_fsw_3::{Log, system};

#[system]
fn logs(logs: [&mut Log; 2]) {
    let _ = logs;
}

fn main() {}
