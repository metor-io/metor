use metor_fsw_3::{Record, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Record, metor_fsw_3::Schema, Serialize, Deserialize)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(max_len = 32)]
struct TwoStamps {
    #[record(timestamp)]
    first: Timestamp,
    #[record(timestamp)]
    second: Timestamp,
}

fn main() {}
