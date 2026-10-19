//! Records shared by the integration tests.

#![allow(dead_code)]

use metor_fsw_3::Record;
use serde::{Deserialize, Serialize};

/// The fixture pack's `Ping` record, declared again here.
///
/// The host matches records by a hash of the name, so this copy is the same
/// record as the pack's.
#[derive(Record, metor_fsw_3::Schema, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(max_len = 8)]
pub struct Ping {
    pub n: u32,
}
