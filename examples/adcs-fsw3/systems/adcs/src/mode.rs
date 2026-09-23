//! The fixed **mode command** of the `adcs-fsw3` target: the phase and pointing
//! law the controller flies, held for the whole run.

use adcs_contracts::ModeCmd;
use metor_fsw_3::{JsonSchema, Output, Timestamp, system};
use serde::Deserialize;

/// The commanded phase and law, both as their [`ModeCmd`] byte values.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ModeParams {
    /// Target phase: `0` idle, `1` settling, `2` pointing, `3` safe, `4` detumble.
    #[serde(default = "default_mode")]
    pub mode: u8,
    /// Pointing law: `0` nadir, `1` velocity-vector, `2` detumble.
    #[serde(default = "default_law")]
    pub law: u8,
}

fn default_mode() -> u8 {
    ModeCmd::POINTING
}

fn default_law() -> u8 {
    ModeCmd::LAW_NADIR
}

/// The commanded mode, written every cycle.
pub struct Mode(ModeCmd);

impl Mode {
    pub fn new(p: ModeParams) -> Self {
        Self(ModeCmd::new(p.mode, p.law))
    }
}

#[system]
impl Mode {
    /// Writes the configured command, stamped with this cycle.
    fn execute(&mut self, now: Timestamp, mode_cmd: &mut Output<ModeCmd>) {
        let _ = mode_cmd.write(&self.0.stamped(now));
    }
}
