//! A pack with three systems, exported over the fsw-3 ABI.

use metor_fsw_3::{Input, Output, Record, SystemTable, system};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The record every system here carries. The host names it `ping` too.
#[derive(Record, Serialize, Deserialize, Clone, Copy, Debug)]
#[record(max_len = 8)]
pub struct Ping {
    pub n: u32,
}

/// Copies its input onto its output.
#[derive(Default)]
pub struct Echo;

#[system]
impl Echo {
    /// Copies the newest ping.
    fn execute(&mut self, input: &mut Input<Ping>, output: &mut Output<Ping>) {
        if let Ok(Some(ping)) = input.latest() {
            let n = ping.n;
            let _ = output.write(&Ping { n });
        }
    }
}

/// Panics from its second cycle on.
#[derive(Default)]
pub struct Boom(u32);

#[system]
impl Boom {
    /// Fails on its second cycle.
    fn execute(&mut self, output: &mut Output<Ping>) {
        self.0 += 1;
        assert!(self.0 < 2, "boom on cycle {}", self.0);
        let _ = output.write(&Ping { n: self.0 });
    }
}

/// `gain`'s params.
#[derive(Deserialize, JsonSchema)]
pub struct GainParams {
    /// Scales every ping.
    pub gain: f64,
}

/// Scales its input by a param.
pub struct Gain(f64);

impl Gain {
    fn new(params: GainParams) -> Self {
        Self(params.gain)
    }
}

#[system]
impl Gain {
    /// Scales the newest ping.
    fn execute(&mut self, input: &mut Input<Ping>, output: &mut Output<Ping>) {
        if let Ok(Some(ping)) = input.latest() {
            let n = (f64::from(ping.n) * self.0) as u32;
            let _ = output.write(&Ping { n });
        }
    }
}

/// The table this pack exports.
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    table.register("echo", Echo::default);
    table.register("boom", Boom::default);
    table.register("gain", Gain::new);
    table
}

metor_fsw_3::export_pack!(pack);
