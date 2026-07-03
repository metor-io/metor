//! The **safe-mode** sequence of the `adcs-fsw2` mission, as a `#[sequence]`-authored
//! `dlopen`-loadable `cdylib` (sequences-slots.md §4). It is the second allowed occupant of
//! the `mode` slot: a one-shot that commands `ModeCmd::safe` and completes. An operator
//! `Load`s it (by name) when the spacecraft should drop to safe.
//!
//! A slot derives a **single** port contract from its allowed set — `resolve` requires every
//! allowed occupant to share the same descriptor shape (sequences-slots.md §5, `resolve_slot`).
//! The `mode` slot's contract carries an `attitude_estimate` input (wired from `nav`), so this
//! occupant must declare the same input even though safing ignores it — hence the unused
//! `_att` port. The commanded `ModeCmd` output is the contract's sole user output.

use adcs_contracts::{AttitudeEstimate, ModeCmd};
use metor_fsw_2::sequence::{now, progress};
use metor_fsw_2::{Input, Outcome, Output};

/// Command safe and complete. `_att` is the contract input the slot shares with
/// `commissioning` (unused here); `mode` is the commanded-mode output.
#[metor_fsw_2::sequence(name = "safe_mode")]
async fn safe(_att: Input<AttitudeEstimate>, mut mode: Output<ModeCmd>) -> Outcome {
    progress("safing");
    mode.publish(&ModeCmd::safe().stamped(now()));
    Outcome::Completed
}
