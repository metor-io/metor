use adcs_contracts::{Mode, ModeCmd};
use metor_fsw_3::{JsonSchema, Output, Timestamp, system};
use serde::Deserialize;

/// Configuration for the mode system.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ModeParams {
    /// The phase to command.
    #[serde(default = "default_mode")]
    pub mode: Mode,
}

fn default_mode() -> Mode {
    Mode::Idle
}

/// The command the mode system writes every cycle.
pub struct ModeSystem(ModeCmd);

impl ModeSystem {
    pub fn new(p: ModeParams) -> Self {
        Self(ModeCmd::new(p.mode))
    }
}

#[system]
/// Writes the configured command stamped `now`.
pub fn mode(#[state] state: &mut ModeSystem, now: Timestamp, mode_cmd: &mut Output<ModeCmd>) {
    let _ = mode_cmd.write(&state.0.stamped(now));
}
