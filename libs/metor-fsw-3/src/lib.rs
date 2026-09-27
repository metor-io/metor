//! metor-fsw is a flight software framework.
//!
//! Systems are functions over typed ring ports, stepped in order by the
//! [`Coordinator`]. `#[system]` on a free fn makes one: each parameter is a
//! port named by its identifier, and one `#[state]` parameter, `&mut S`, is
//! the state the registered constructor builds. The fn's name is the
//! system's name, and its doc comment is the system's doc.
//!
//! ```
//! use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//! use metor_fsw_3::{Frame, Input, Output, SystemTable, Timestamp, system};
//!
//! #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! #[repr(C)]
//! struct Gps {
//!     #[frame(timestamp)]
//!     timestamp: Timestamp,
//!     pos: [f64; 3],
//! }
//!
//! #[derive(Default)]
//! struct NavState {
//!     fixes: u64,
//! }
//!
//! /// Counts fixes and forwards the newest.
//! #[system]
//! fn nav(#[state] s: &mut NavState, gps: &mut Input<Gps>, out: &mut Output<Gps>) {
//!     if let Ok(Some(fix)) = gps.latest() {
//!         s.fixes += 1;
//!         let _ = out.write(&Gps { timestamp: fix.timestamp, pos: fix.pos });
//!     }
//! }
//!
//! /// Publishes the origin each cycle.
//! #[system]
//! fn origin(now: Timestamp, out: &mut Output<Gps>) {
//!     let _ = out.write(&Gps { timestamp: now, pos: [0.0; 3] });
//! }
//!
//! let mut table = SystemTable::new();
//! table.register("nav", nav(), NavState::default)?;
//! table.register("origin", origin(), Default::default)?;
//! # Ok::<(), metor_fsw_3::BuildError>(())
//! ```
//!
//! A fn without `#[state]` is stateless and registers with
//! `Default::default`. `nav()` derefs to a fn pointer, so a test calls the
//! system directly: `nav()(&mut state, &mut gps, &mut out)`.
//!
//! An `async fn` ending in `stop: Stop` instead runs on a background thread
//! behind the [`thread`] adapter, which mirrors its ports. It has no direct
//! call; a test drives it through [`AsyncSystemFn::call`].
//!
//! ```
//! # use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//! # use metor_fsw_3::{Frame, Timestamp};
//! # #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! # #[repr(C)]
//! # struct Gps {
//! #     #[frame(timestamp)]
//! #     timestamp: Timestamp,
//! #     pos: [f64; 3],
//! # }
//! use metor_fsw_3::ring::Notifier;
//! use metor_fsw_3::{Input, Output, Stop, SystemTable, system};
//!
//! /// Copies every fix until stop.
//! #[system]
//! async fn relay(gps: &mut Input<Gps, Notifier>, out: &mut Output<Gps>, stop: Stop) {
//!     while !stop.is_set() {
//!         let fix = futures_lite::future::or(async { gps.next().await.ok() }, async {
//!             stop.wait().await;
//!             None
//!         }).await;
//!         let Some(fix) = fix else { return };
//!         let fix = Gps { timestamp: fix.timestamp, pos: fix.pos };
//!         let _ = out.write(&fix);
//!     }
//! }
//!
//! let mut table = SystemTable::new();
//! table.register_async("relay", relay(), Default::default)?;
//! # Ok::<(), metor_fsw_3::BuildError>(())
//! ```
//!
//! An `async fn` under `#[sequence]` is polled once per cycle instead, and
//! returns when its steps are done. Its [`Seq`] handle reads the cycle's
//! time and waits in cycle time; an abort ends any wait with
//! [`SeqError::Aborted`].
//!
//! ```
//! # use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//! # use metor_fsw_3::{Frame, Timestamp};
//! # #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! # #[repr(C)]
//! # struct Gps {
//! #     #[frame(timestamp)]
//! #     timestamp: Timestamp,
//! #     pos: [f64; 3],
//! # }
//! use core::time::Duration;
//! use metor_fsw_3::{Input, Output, Seq, SeqError, SystemTable, sequence};
//!
//! /// Waits for a fix, then publishes the origin for one second.
//! #[sequence]
//! async fn hold(seq: &Seq, gps: &mut Input<Gps>, out: &mut Output<Gps>) -> Result<(), SeqError> {
//!     seq.wait_for(Duration::from_secs(5), || gps.latest().is_ok_and(|f| f.is_some()))
//!         .await?;
//!     let _ = out.write(&Gps { timestamp: seq.now(), pos: [0.0; 3] });
//!     seq.sleep(Duration::from_secs(1)).await
//! }
//!
//! let mut table = SystemTable::new();
//! table.register_sequence("hold", hold(), Default::default)?;
//! # Ok::<(), metor_fsw_3::BuildError>(())
//! ```
//!
//! `channel=` in a system's config wraps it in the [`channel`] adapter, which
//! starts, stops, resets, aborts, and loads it on `SequenceCommand`s; a
//! [`dl`] slot loads pack systems into one place in the schedule.
//!
//! The built-in [`link`] systems, `Publish` and `Subscribe`, are async
//! systems with config-listed ports; they carry a target's records over
//! metor-proto to whoever connects, and its commands back.

// A pack build compiles the definitions the host's build shares, unused.
#![cfg_attr(not(feature = "host"), allow(dead_code))]

// The derives name this crate by its package name, in doctests as well as here.
extern crate self as metor_fsw_3;

pub mod async_system;
pub mod channel;
#[cfg(feature = "host")]
pub mod cli;
pub mod coordinator;
pub mod def;
#[cfg(feature = "host")]
pub mod deploy;
#[cfg(feature = "host")]
pub mod dl;
pub mod fn_system;
pub mod frame;
#[cfg(feature = "host")]
pub mod link;
pub mod log;
pub mod pack;
mod panic;
pub mod port;
pub mod record;
pub mod sequence;
pub mod system;
pub mod thread;

#[cfg(test)]
mod tests;

pub use async_system::{AsyncSystem, Stop};
pub use channel::{Abort, AbortRequest, ChannelState, ChannelStatus};
#[cfg(feature = "host")]
pub use coordinator::Coordinator;
pub use coordinator::{
    Adapter, BuildError, ChannelConfig, Clock, CoordinatorConfig, EndReason, InputConfig,
    LoadError, Occupant, OutputConfig, OutputSource, ParamError, Params, PeerLink, PeerPort,
    PortRef, RunState, Step, SystemConfig, SystemKind, SystemStatus, SystemTable,
};
pub use def::{DefCx, DefError, Records};
#[cfg(feature = "host")]
pub use dl::{DlPack, DlStep, Pack, PackError};
pub use fn_system::{
    AsyncSystemFn, Ctor, Cycle, FnSystem, InSet, OutSet, Param, ParamNames, Ports, SystemFn,
};
pub use frame::Frame;
#[cfg(feature = "host")]
pub use link::{LinkStatus, Transport};
pub use log::{Log, LogLayer};
pub use metor_fsw_3_macros::{Frame, Record, sequence, system};
pub use metor_fsw_3_ring::{ReadError, WriteError};
pub use pack::def::{PackDef, PackSystemDef};
pub use pack::{ABI_VERSION, Status};
pub use port::{
    DynInputs, DynOutputs, Input, Latest, Output, RecvError, Sample, SendError, ring_capacity,
};
pub use record::{Bytes, DecodeError, EncodeError, MsgCodec, Record, RecordSchema};
pub use sequence::{OnAbort, Redline, Seq, SeqError, SequenceFn};
pub use system::{
    InputBinding, OutputBinding, PortDef, System, SystemDef, SystemInputs, SystemOutputs,
};

pub use metor_fsw_3_ring as ring;
pub use metor_proto::types::Timestamp;

pub use metor_component::path;
pub use metor_component::{
    AsVTable, Componentize, Decomponentize, FrameBytes, FrameStr, FrameStrError, Metadatatize,
};
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
