//! Records the unit tests share.

use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Frame;

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "imu")]
#[repr(C)]
pub struct Imu {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub sample: f64,
}

impl Imu {
    pub fn new(timestamp: i64, sample: f64) -> Self {
        Self {
            timestamp: Timestamp(timestamp),
            sample,
        }
    }
}

/// A frame with no `name`, so its name is the snake-cased ident.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct BareName {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub value: u64,
}

/// A message with a static postcard bound, so it states no length.
#[derive(
    crate::Record, crate::MaxSize, crate::Schema, Serialize, Deserialize, Debug, PartialEq,
)]
pub struct Fixed {
    pub a: u32,
    pub b: f64,
}

/// A message with a string, so it states its length.
#[derive(crate::Record, crate::Schema, Serialize, Deserialize, Debug, PartialEq)]
#[record(max_len = 64, depth = 4)]
pub struct Note {
    pub text: String,
}
