use adcs_contracts::ModeCmd;
use metor_fsw_3::{JsonSchema, Output, Timestamp, system};
use serde::Deserialize;

/// Configuration for the mode system, as [`ModeCmd`] byte values.
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

/// The command the mode system writes every cycle.
pub struct Mode(ModeCmd);

impl Mode {
    pub fn new(p: ModeParams) -> Self {
        Self(ModeCmd::new(p.mode, p.law))
    }
}

#[system]
/// Writes the configured command stamped `now`.
pub fn mode(#[state] state: &mut Mode, now: Timestamp, mode_cmd: &mut Output<ModeCmd>) {
    let _ = mode_cmd.write(&state.0.stamped(now));
}
