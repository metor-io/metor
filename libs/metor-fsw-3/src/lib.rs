//! metor-fsw is a framework for writing flight software.
//!
//! Data moves through it as records. A [`Frame`] is a fixed-layout struct sent
//! as its raw bytes. Any other [`Record`] is encoded with postcard or JSON.
//! Both are derived:
//!
//! ```
//! use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//! use metor_fsw_3::{Frame, Timestamp};
//!
//! #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! #[repr(C)]
//! struct Gps {
//!     #[frame(timestamp)]
//!     timestamp: Timestamp,
//!     pos: [f64; 3],
//! }
//! ```

// The derives name this crate by its package name, in doctests as well as here.
extern crate self as metor_fsw_3;

pub mod frame;
pub mod record;

#[cfg(test)]
mod tests;

pub use frame::Frame;
pub use metor_fsw_3_macros::{Frame, Record};
pub use record::{Bytes, DecodeError, EncodeError, MsgCodec, Payload, Record, RecordSchema};

pub use metor_proto::types::Timestamp;

pub use metor_component::path;
pub use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_proto;
pub use metor_proto_wkt;
pub use postcard;
pub use postcard::experimental::max_size::MaxSize;
pub use postcard_schema;
pub use postcard_schema::Schema;
pub use serde;
pub use serde_json;
pub use zerocopy;
