//! The authoring surface of metor-fsw-2: frames, ports, systems, and packs.
//!
//! This crate is everything a system author writes against, and nothing that
//! flies a target. `metor-fsw-2` links it, adds the coordinator, the wiring
//! resolver, the loaders, and the CLI, and re-exports it whole — so a target
//! crate still sees one surface, while a pack, a sequence, or a contract crate
//! depends on this crate alone.
//!
//! The boundary is a build constraint, not a taste: this crate compiles for
//! `wasm32-unknown-unknown`, which the host cannot. A sequence that runs as a
//! wasm guest reaches its ports through exactly the types below.
//!
//! # Data
//!
//! A [`Frame`] is a `#[repr(C)]` struct whose fields share one timestamp. Its
//! memory bytes are also its ring and wire bytes. A vtable describes those
//! fields to code that does not know the Rust type.
//!
//! ```edition2021
//! use metor_fsw_2_core::*;
//! use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//!
//! #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! #[repr(C)]
//! #[metor_fsw(name = "imu")]
//! struct Imu {
//!     #[metor_fsw(timestamp)]
//!     timestamp: Timestamp,
//!     omega: f64,
//!     accel: f64,
//! }
//!
//! fn main() {
//!     let imu = Imu { timestamp: Timestamp(42), omega: 0.1, accel: 9.8 };
//!     assert_eq!(Imu::NAME, "imu");
//!     assert_eq!(imu.timestamp(), Timestamp(42));
//! }
//! ```
//!
//! [`FrameList`] and [`FrameMap`] add a bounded variable-size trailer. Their
//! const bounds keep the worst-case record size known when the coordinator
//! sizes a ring. [`FrameWriter`] builds these records.
//!
//! Commands and events use postcard messages through [`MsgOut`] and [`MsgIn`].
//! Message inputs may accept many producers. Each producer still owns its own
//! single-writer ring.
//!
//! # Systems
//!
//! Three authoring forms live here:
//!
//! - A function passed to [`system()`] runs once per cycle. Its arguments
//!   declare its ports.
//! - A function passed to [`Pack::task`] owns its ports in a future. The driver
//!   polls that future once per cycle. Sequences use this form.
//! - A [`CyclicSystem`] struct runs once per cycle.
//!
//! The fourth form, `AsyncSystem`, is a free-running task the host's executor
//! owns; it lives in `metor-fsw-2` because no pack can construct one.
//!
//! An [`Output`] owns a ring writer. An [`Input`] owns a read view into an
//! upstream ring. A full ring makes a write return [`WriteError`]. The
//! `publish` helpers keep the cycle moving by dropping the new record and
//! counting that loss for health telemetry.
//!
//! Each system has health and log outputs. Cyclic drivers close a health cycle
//! after each step and send a [`SystemHealth`] frame plus queued [`LogEvent`]
//! messages.
//!
//! # Packs
//!
//! A [`Pack`] lists the systems one crate exports. The host can link a pack,
//! load its `cdylib` through `dl`, or run an entry in a worker process. All
//! three paths use the same descriptors, the same [`RingSource`] binding, and
//! the same [`CyclicSlot`] step interface, so a system never learns which one
//! it is under.
//!
//! # More detail
//!
//! Start with [`docs/README.md`](https://github.com/metor-io/metor/blob/main/libs/metor-fsw-2/docs/README.md).

mod binder;
mod descriptor;
mod dynamic;
mod frame;
mod handler;
mod message;
mod pack;
mod port;
mod registry;
mod shared;
mod slot;
mod system;
mod text;
mod writer;

mod clock;
pub mod health;
pub mod logfwd;

pub mod sequence;

pub mod abi;
mod params_docs;

/// Schema- and serde-guided decoding of a system's params off a value tree,
/// shared by the host's static registry and every pack entry's create phase.
pub mod params;

pub use dynamic::{FrameList, FrameMap};
pub use frame::Frame;
pub use text::FrameStr;
pub use writer::{DynamicWriteError, FrameWriter, KeyError, ListWriter, MapWriter};

pub use metor_fsw_ring::{ReadError, WriteError};

pub use binder::{AnySource, BindPorts, Binder, BoundInput, BoundPort, RingSource};
pub use handler::{
    AsyncSystemFn, BindCx, CycleCx, DeclSink, ExecParam, ExecParamSet, ExecuteFn, InitFn,
    IntoOutcome, IntoPackEntry, Params, SystemDef, TaskParam, system,
};
pub use pack::{
    AttachTarget, Created, Driver, EntryParams, MakeError, Mount, Pack, PackEntry, Pending,
    StepStatus,
};
pub use registry::{AllOutputs, Registry, RegistryEntry};
#[doc(hidden)]
pub use shared::{AlreadySet, ErasedShared, SharedCell};
pub use shared::{Shared, SharedLifecycle};
pub use slot::{
    CyclicSlot, NAME_CAP, SlotState, StopReason, StoppedSystem, WorkerRunState, WorkerStatus,
};

pub use descriptor::{
    Capability, Declarations, Delivery, FanIn, Hz, PortConn, PortDesc, PortId, PortSchema,
    SystemDescriptor, SystemKind,
};
pub use health::{HealthPort, LogEvent, LogLevel, MAX_ERR_KINDS, MAX_LINES, SystemHealth};
pub use port::{
    DEFAULT_DEPTH, FrameRef, FrameWriteError, Input, Output, buffer_capacity, capacity_for,
};

pub use message::{
    CommandOut, MAX_MSG_BYTES, MsgFanOut, MsgIn, MsgOut, MsgTable, NamedMsg, split_record,
};
#[doc(hidden)]
pub use params_docs::ParamsDocEntry;
pub use system::{
    BuildCtx, BuildSystem, ConfigureError, CyclicRunner, CyclicSystem, HealthOutput, Out, System,
    SystemInput, SystemOutput, descriptor_for,
};

// The seams the host crate binds against: the pack runtime's slot adapter and
// state table, the registry the coordinator assembles, the frame helpers its
// slots reuse, and the cycle clock. Not part of the authoring surface.
#[doc(hidden)]
pub use clock::{now_or_wall, set_now};
#[doc(hidden)]
pub use descriptor::compatible;
#[doc(hidden)]
pub use message::LOG_DEPTH;
#[doc(hidden)]
pub use pack::{CreateFn, DriverSlot, StateEntry, decode_params, resolve_defaults};
#[doc(hidden)]
pub use port::{drain_view, frame_list_iter};

// Re-exported so `#[derive(inventory-based ParamsDocs)]` can name the crate.
#[doc(hidden)]
pub use inventory;

pub use metor_fsw_ring as ring;

pub use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_fsw_2_macros::{Frame, ParamsDocs, SystemInput, SystemOutput};

pub use metor_proto::types::Timestamp;

pub use sequence::{Outcome, SequenceStatus, SlotControlIn};

pub use metor_component::path;
pub use {metor_proto, metor_proto_wkt, zerocopy};

// Frame acceptance tests span frame/dynamic/writer, so they live at the
// crate root rather than under any single module.
#[cfg(test)]
mod tests;
