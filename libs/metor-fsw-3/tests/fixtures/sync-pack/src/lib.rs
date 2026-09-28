//! One stepped system, so the pack builds as a module.

use metor_fsw_3::{Input, Output, Record, SystemTable, system};
use serde::{Deserialize, Serialize};

#[derive(Record, metor_fsw_3::Schema, Serialize, Deserialize, Clone, Copy, Debug)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(max_len = 8)]
pub struct Ping {
    pub n: u32,
}

/// Copies the newest ping.
#[system]
fn echo(input: &mut Input<Ping>, output: &mut Output<Ping>) {
    if let Ok(Some(ping)) = input.latest() {
        let _ = output.write(&Ping { n: ping.n });
    }
}

/// The table this pack exports.
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    // PANIC Safety: one system cannot conflict with itself.
    table
        .register("echo", echo(), Default::default)
        .expect("valid records");
    table
}

metor_fsw_3::export_pack!(pack);
