//! The **safe-mode** sequence of the `adcs-fsw2` mission: an async fn registered in this
//! crate's [`pack`], built as a `dlopen`-loadable `cdylib`. It is the second allowed
//! occupant of the `mode` slot: a one-shot that commands `ModeCmd::safe` and completes. An
//! operator `Load`s it (by name) when the spacecraft should drop to safe.
//!
//! A slot derives a **single** port contract from its allowed set — `resolve` requires every
//! allowed occupant to share the same descriptor shape (sequences-slots.md §5, `resolve_slot`).
//! The `mode` slot's contract carries `attitude_estimate` and `gps` inputs (wired from `nav`
//! and `plant` — commissioning's condition gates read them), so this occupant must declare
//! the same inputs in the same order even though safing ignores both — hence the unused
//! `_att`/`_gps` ports. The commanded `ModeCmd` output is the contract's sole user output.

use adcs_contracts::{AttitudeEstimate, Gps, ModeCmd};
use metor_fsw_2::sequence::{now, progress};
use metor_fsw_2::{Input, Outcome, Output};

/// Command safe and complete. `_att`/`_gps` are the contract inputs the slot shares with
/// `commissioning` (unused here); `mode` is the commanded-mode output.
async fn safe(
    _att: Input<AttitudeEstimate>,
    _gps: Input<Gps>,
    mut mode: Output<ModeCmd>,
) -> Outcome {
    progress("safing");
    mode.publish(&ModeCmd::safe().stamped(now()));
    Outcome::Completed
}

/// This crate's pack: the safing sequence as its sole entry, under the name the mission's
/// slot `allow` line selects (`occupant="safe_mode"`).
pub fn pack() -> metor_fsw_2::Pack {
    metor_fsw_2::Pack::new().sequence("safe_mode", safe)
}
metor_fsw_2::export_pack!(pack);
