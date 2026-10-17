//! The `seqs` pack: sequences that command the `adcs` pack's controller.
//!
//! It needs only the contracts' frames, so it builds as a wasm module as well
//! as a library.

use core::time::Duration;

use adcs_contracts::{AttitudeEstimate, ModeCmd, V3};
use metor_fsw_3::{JsonSchema, Output, Sample, Seq, SeqError, SystemTable, sequence};
use serde::Deserialize;

/// Configuration for [`commission`].
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct CommissionParams {
    /// Estimated body rate (rad/s) under which detumbling has settled.
    pub rate: f64,
    /// Longest detumble (s) before going on to nadir regardless.
    pub detumble_timeout_s: f64,
}

/// [`commission`]'s params, parsed from [`CommissionParams`].
///
/// A rate or timeout that is negative or not finite does not parse.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(try_from = "CommissionParams")]
pub struct Commission {
    rate: f64,
    detumble_timeout: Duration,
}

impl TryFrom<CommissionParams> for Commission {
    type Error = String;

    fn try_from(p: CommissionParams) -> Result<Self, String> {
        if !(p.rate.is_finite() && p.rate >= 0.0) {
            return Err(format!("`rate` is {}", p.rate));
        }
        let detumble_timeout = Duration::try_from_secs_f64(p.detumble_timeout_s)
            .map_err(|err| format!("`detumble_timeout_s`: {err}"))?;
        Ok(Self {
            rate: p.rate,
            detumble_timeout,
        })
    }
}

impl JsonSchema for Commission {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        CommissionParams::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        CommissionParams::json_schema(generator)
    }
}

/// Detumbles on the magnetorquers until the estimated rate settles, then
/// holds nadir. Commands safe if aborted.
#[sequence]
pub async fn commission(
    #[state] params: &mut Commission,
    seq: &Seq,
    estimate: &Sample<AttitudeEstimate>,
    mode_cmd: &mut Output<ModeCmd>,
) -> Result<(), SeqError> {
    let _safe = seq.on_abort(|seq| {
        let _ = mode_cmd.write(&ModeCmd::safe().stamped(seq.now()));
    });
    let _ = mode_cmd.write(&ModeCmd::detumble().stamped(seq.now()));
    let settled = || {
        estimate
            .get()
            .is_some_and(|e| below(&e.omega_b, params.rate))
    };
    // A slow detumble goes on to nadir, as a boot sequence would.
    let _ = seq.wait_for(params.detumble_timeout, settled).await;
    let _ = mode_cmd.write(&ModeCmd::idle().stamped(seq.now()));
    Ok(())
}

/// Whether `|v| < bound`, summed in scalar order.
///
/// `V3::norm` goes through `faer`, whose wasm build carries SIMD the module
/// runtime refuses, and whose native build may round differently.
fn below(v: &V3, bound: f64) -> bool {
    let [x, y, z] = v.into_buf();
    (x * x + y * y + z * z).sqrt() < bound
}

/// Returns a table of the pack's sequences, registered as `commission`.
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    // PANIC Safety: these static record schemas are compatible.
    table
        .register_sequence(
            "commission",
            commission(),
            core::convert::identity::<Commission>,
        )
        .expect("valid records");
    table
}

#[cfg(feature = "export")]
metor_fsw_3::export_pack!(pack);
