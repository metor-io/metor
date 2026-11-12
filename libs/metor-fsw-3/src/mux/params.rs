//! The `fsw.mux` config params, as the Python `Mux` builtin writes them.

use core::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

use super::cal::TcKind;

/// Every input port and output of one mux.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct MuxParams {
    pub inputs: Vec<String>,
    pub outputs: Vec<OutParam>,
}

/// One output port: when it emits, how it is stamped, and its fields.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct OutParam {
    pub port: String,
    pub on: TriggerParam,
    pub stamp: StampParam,
    /// The output is withheld while a mapped input is older than this, by
    /// this target's clock.
    #[serde(default)]
    pub max_age: Option<Duration>,
    pub fields: Vec<FieldMap>,
}

/// When an output emits.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TriggerParam {
    /// Once per record drained from this input.
    Input(String),
    /// Once per cycle in which any mapped input has a new record.
    Any,
    /// Once every mapped input has a record newer than the last emit.
    All,
    /// Every cycle.
    Cycle,
}

/// Where an output's stamp comes from.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StampParam {
    /// The latest stamp of this input.
    Input(String),
    /// The cycle time.
    Now,
}

/// Copies input `input`'s field `src` to the output's field `dst`, through
/// `cal` when set.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct FieldMap {
    pub dst: String,
    pub input: String,
    pub src: String,
    #[serde(default)]
    pub cal: Option<CalParam>,
}

/// A calibration from a scalar `f64` to a scalar `f64`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CalParam {
    /// Ascending coefficients.
    Poly(Vec<f64>),
    /// `(x, y)` points in strictly increasing `x`.
    Table(Vec<(f64, f64)>),
    /// Cold-junction-compensated volts to °C.
    Thermocouple { kind: TcKind },
    /// `src / excitation` scaled from `span` to `eng`; `excitation` is the
    /// path of a scalar `f64` in the mapping's own input.
    Ratiometric {
        excitation: String,
        span: (f64, f64),
        eng: (f64, f64),
    },
}
