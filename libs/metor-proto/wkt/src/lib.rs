use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};

mod image;
mod metadata;
mod msgs;
mod path;
mod tile;
#[cfg(feature = "nox")]
mod value;

pub use image::*;
pub use metadata::*;
pub use msgs::*;
pub use path::*;
pub use tile::*;
#[cfg(feature = "nox")]
pub use value::*;

#[derive(Clone, Serialize, Deserialize, Copy)]
pub struct LastUpdated(pub Timestamp);
