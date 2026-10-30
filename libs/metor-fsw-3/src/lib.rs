//! metor-fsw is a framework for writing flight software.
//!
//! Flight software is built from systems. A system is a function that reads
//! and writes typed ports, which are backed by ring buffers. The
//! [`Coordinator`] runs every system in a fixed order, once per cycle.
//!
//! The usual way to write a system is `#[system]` on a free fn. Each parameter
//! is a port, named after the parameter. A `#[state] s: &mut S` parameter gives
//! the system state, built by the constructor passed at registration. The fn's
//! name becomes the system's name, and its doc comment becomes the system's doc.
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
//! A failed write, for example into a full ring, returns an error. The
//! failure is also counted. After the cycle, the system's `log` gets one
//! `publish_dropped` fault covering every failed write.
//!
//! A fn without `#[state]` is stateless and registers with
//! `Default::default`. `nav()` derefs to a fn pointer, so a test can call the
//! system directly: `nav()(&mut state, &mut gps, &mut out)`.
//!
//! An `async fn` whose last parameter is `stop: Stop` becomes an async system.
//! It runs on a background thread, and the [`thread`] adapter copies its port
//! data to and from the schedule. It cannot be called directly, so a test
//! drives it through [`AsyncSystemFn::call`].
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
//! A sequence is an `async fn` marked `#[sequence]`. The coordinator polls it
//! once per cycle until it returns. Its [`Seq`] handle gives it the cycle's
//! time and lets it wait in cycle time. An abort ends any wait with
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
//! Setting `channel=` in a system's config wraps the system in the [`channel`]
//! adapter. A channel lets an operator start, stop, reset, abort, or load the
//! system by sending `SequenceCommand` messages. A [`dl`] slot is a place in the
//! schedule whose system can be swapped for another pack system at run time.
//!
//! The built-in [`link`] systems, `Publish` and `Subscribe`, move records
//! between targets. They are async systems whose ports are listed in config.
//! `Publish` sends records over metor-proto to every connected peer.
//! `Subscribe` writes the records peers send onto its outputs. Together they
//! carry a target's telemetry out and its commands back.

// The derives name this crate by its package name, in doctests as well as here.
extern crate self as metor_fsw_3;

pub mod async_system;
#[cfg(feature = "host")]
pub mod builtins;
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
pub mod frame_map;
pub mod io;
#[cfg(feature = "host")]
pub mod link;
pub mod log;
#[cfg(all(feature = "threads", not(target_family = "wasm")))]
mod mirror;
pub mod pack;
mod panic;
pub mod port;
pub mod record;
mod rng;
pub mod sequence;
pub mod system;
pub mod thread;
#[cfg(feature = "host")]
pub mod wasm;

#[cfg(test)]
mod tests;

pub use async_system::{AsyncSystem, Stop};
pub use channel::{Abort, AbortRequest, ChannelMessage, ChannelStatus};
#[cfg(feature = "host")]
pub use coordinator::Coordinator;
pub use coordinator::{
    Adapter, BuildError, ChannelConfig, ChannelName, ChannelNameTooLong, Clock, CoordinatorConfig,
    EndReason, InputConfig, LoadError, MakeError, Occupant, OutputConfig, OutputSource, ParamError,
    Params, PortRef, RunState, Step, SystemConfig, SystemKind, SystemStatus, SystemTable, WallRate,
};
pub use def::{DefCx, DefError, Records};
#[cfg(feature = "host")]
pub use dl::{DlPack, DlStep, Pack, PackError};
pub use fn_system::{
    AsyncSystemFn, Ctor, Cycle, FnSignature, FnSystem, InSet, OutSet, Param, ParamNames, SystemFn,
};
pub use frame::Frame;
pub use frame_map::{FrameRef, FrameWriteError, FrameWriter, MapRef, MapWriter};
pub use io::{Buf, Cap, Handle, Io, IoInit, Pending, ResourceStatus, Slot};
#[cfg(feature = "host")]
pub use link::{LinkParams, LinkStatus};
pub use log::{Log, LogLayer};
pub use metor_fsw_3_macros::{Frame, Record, sequence, system};
pub use metor_fsw_3_ring::{ReadError, WriteError};
pub use pack::def::{PackDef, PackSystemDef};
pub use pack::{ABI_VERSION, Status};
pub use port::{
    DynInputs, DynOutputs, Input, InputWake, Latest, Output, RecvError, Sample, SendError,
    ring_capacity,
};
pub use record::{Bytes, DecodeError, EncodeError, MsgCodec, Payload, Record, RecordSchema};
pub use rng::Rng;
pub use sequence::{OnAbort, Redline, Seq, SeqError, SequenceFn};
pub use system::{BindCx, InputBinding, OutputBinding, PortDef, Ports, System, SystemDef};

pub use metor_fsw_3_ring as ring;
pub use metor_proto::types::Timestamp;

pub use metor_component::path;
pub use metor_component::{
    AsVTable, Componentize, Decomponentize, Element, FrameArray, FrameBytes, FrameImage, FrameMap,
    FrameStr, FrameStrError, KeyError, MapKey, Metadatatize, Mono8, Mono16, Pixel, Rgb8, Rgba8,
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
