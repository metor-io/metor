//! metor-fsw is a flight software framework.
//!
//! Systems are functions over typed ring ports, stepped in order by the
//! [`Coordinator`]. A system authored as `async fn run` instead runs on a
//! background thread behind the [`thread`] adapter, which mirrors its ports:
//!
//! ```
//! use metor_fsw_3::ring::Notifier;
//! use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//! use metor_fsw_3::{Frame, Input, Output, Stop, SystemTable, Timestamp, system};
//!
//! #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! #[repr(C)]
//! struct Gps {
//!     #[frame(timestamp)]
//!     timestamp: Timestamp,
//!     pos: [f64; 3],
//! }
//!
//! struct Relay;
//!
//! #[system]
//! impl Relay {
//!     /// Copies every fix until stop.
//!     async fn run(&mut self, gps: &mut Input<Gps, Notifier>, out: &mut Output<Gps>, stop: Stop) {
//!         while !stop.is_set() {
//!             let fix = futures_lite::future::or(async { gps.next().await.ok() }, async {
//!                 stop.wait().await;
//!                 None
//!             }).await;
//!             let Some(fix) = fix else { return };
//!             let fix = Gps { timestamp: fix.timestamp, pos: fix.pos };
//!             let _ = out.write(&fix);
//!         }
//!     }
//! }
//!
//! let mut table = SystemTable::new();
//! table.register_async("relay", || Relay)?;
//! # Ok::<(), metor_fsw_3::BuildError>(())
//! ```
//!
//! The built-in [`link`] systems, `Publish` and `Subscribe`, are async
//! systems with config-listed ports; they carry a target's records over
//! metor-proto to whoever connects, and its commands back.

// The derives name this crate by its package name, in doctests as well as here.
extern crate self as metor_fsw_3;

pub mod async_system;
pub mod cli;
pub mod coordinator;
pub mod def;
pub mod dl;
pub mod fn_system;
pub mod frame;
pub mod link;
pub mod log;
pub mod pack;
mod panic;
pub mod port;
pub mod record;
pub mod system;
pub mod thread;

#[cfg(test)]
mod tests;

pub use async_system::{AsyncSystem, Stop};
pub use coordinator::{
    BuildError, Clock, Coordinator, CoordinatorConfig, InputConfig, OutputConfig, ParamError,
    Params, PortRef, Step, SystemConfig, SystemStatus, SystemTable,
};
pub use def::{DefCx, DefError, Records};
pub use dl::{DlStep, Pack, PackError, PackFns};
pub use fn_system::{
    AsyncSystemFn, Ctor, Cycle, FnAsyncSystem, FnSystem, InSet, OutSet, Param, ParamNames, Ports,
    SystemFn,
};
pub use frame::Frame;
pub use link::{LinkStatus, Transport};
pub use log::{Log, LogLayer};
pub use metor_fsw_3_macros::{Frame, Record, SystemInputs, SystemOutputs, system};
pub use metor_fsw_3_ring::{ReadError, WriteError};
pub use pack::def::{PackDef, PackSystemDef};
pub use pack::{ABI_VERSION, Status};
pub use port::{DynInputs, DynOutputs, Input, Latest, Output, RecvError, SendError, ring_capacity};
pub use record::{Bytes, DecodeError, EncodeError, MsgCodec, Record, RecordSchema};
pub use system::{
    InputBinding, OutputBinding, PortDef, System, SystemDef, SystemInputs, SystemOutputs,
};

pub use metor_fsw_3_ring as ring;
pub use metor_proto::types::Timestamp;

pub use metor_component::path;
pub use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_proto;
pub use metor_proto_wkt;
pub use postcard;
pub use postcard::experimental::max_size::MaxSize;
pub use postcard_schema;
pub use postcard_schema::Schema;
pub use schemars::{self, JsonSchema};
pub use serde;
pub use serde_json;
pub use tracing;
pub use zerocopy;
