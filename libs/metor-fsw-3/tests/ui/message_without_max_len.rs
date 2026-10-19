use metor_fsw_3::Record;
use metor_fsw_3::serde::{Deserialize, Serialize};

#[derive(Record, Serialize, Deserialize)]
struct Unbounded {
    text: String,
}

fn main() {}
