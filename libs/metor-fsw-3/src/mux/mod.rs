//! `fsw.mux`: builds frames from the fields of other frames.
//!
//! Each output is a list of field copies from the mux's inputs, resolved at
//! make against the record registry. See `docs/muxes.md`.

pub mod cal;
pub mod params;
pub mod plan;
mod step;

pub use cal::{Cal, TcKind};
pub use params::{CalParam, FieldMap, MuxParams, OutParam, StampParam, TriggerParam};
pub use plan::{MuxError, Plan};
pub use step::Mux;

use metor_proto::types::Timestamp;

use crate::port::{DynInputs, DynOutputs};

/// Builds each output from fields of the inputs, as its params map them.
#[crate::system]
pub fn mux(#[state] s: &mut Mux, now: Timestamp, inputs: &mut DynInputs, outputs: &mut DynOutputs) {
    s.step(now, inputs, outputs);
}

impl From<MuxError> for crate::coordinator::MakeError {
    fn from(e: MuxError) -> Self {
        Self::Mux {
            message: e.to_string(),
        }
    }
}
