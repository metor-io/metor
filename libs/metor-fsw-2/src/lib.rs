//! Build flight software as a checked graph of small systems.
//!
//! The coordinator owns the graph, its ring buffers, and the cycle clock. Each
//! system reads typed inputs and writes typed outputs. The wiring pass checks the
//! graph before the first cycle runs.
//!
//! ```text
//!            coordinator (sizes rings, binds ports, drives the cycle)
//!                |
//!   [imu] -Imu-> [nav] -NavState-> [ctrl] -TorqueCmd-> [actuators]
//!     \_______________ ring buffers _______________/
//!                |
//!           [downlink] --> TCP --> ground
//! ```
//!
//! # Data
//!
//! A [`Frame`] is a `#[repr(C)]` struct whose fields share one timestamp. Its
//! memory bytes are also its ring and wire bytes. A vtable describes those
//! fields to code that does not know the Rust type.
//!
//! ```edition2021
//! use metor_fsw_2::*;
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
//! The crate has four authoring forms:
//!
//! - A function passed to [`system()`] runs once per cycle. Its arguments declare
//!   its ports.
//! - A function passed to [`Pack::task`] owns its ports in a future. The driver
//!   polls that future once per cycle. Sequences use this form.
//! - A [`CyclicSystem`] struct runs once per cycle.
//! - An [`AsyncSystem`] struct owns a free-running task and waits on input or
//!   time. The coordinator does not poll it once per cycle.
//!
//! An [`Output`] owns a ring writer. An [`Input`] owns a read view into an
//! upstream ring. A full ring makes a write return [`WriteError`]. The
//! `publish` helpers keep the cycle moving by dropping the new record and
//! counting that loss for health telemetry.
//!
//! Each system has health and log outputs. Cyclic drivers close a health cycle
//! after each step and send a [`SystemHealth`] frame plus queued [`LogEvent`]
//! messages. A free-running [`AsyncSystem`] controls when it sends its own
//! health data.
//!
//! # Wiring and loading
//!
//! A `target.py` file and [`WiringBuilder`] both create the same [`Wiring`] IR.
//! The resolver checks that IR, loads
//! each system descriptor, builds the graph, sizes its rings, and returns a
//! ready [`Coordinator`].
//!
//! A [`Pack`] lists the systems one crate exports. The host can link a pack,
//! load its `cdylib` in the host through [`dl`], or run an entry in a worker
//! through [`proc`]. All three paths use the same descriptors and port rules.
//!
//! # More detail
//!
//! Start with [`docs/README.md`](https://github.com/metor-io/metor/blob/main/libs/metor-fsw-2/docs/README.md).
//! It links to focused design docs for frames, systems, wiring, loading,
//! process workers, telemetry, alarms, and runtime slots.

mod alarm;
mod binder;
mod coordinator;
mod descriptor;
mod dynamic;
mod frame;
mod handler;
mod message;
mod pack;
mod port;
mod preset;
mod registry;
mod shared;
mod system;
mod telemetry;
mod text;
mod writer;

mod clock;
pub mod health;
pub mod logfwd;

pub mod sequence;

// The pure-data target IR; the `wiring` module re-exports these types
// alongside its resolver.
pub mod ir;

pub mod wiring;

pub mod abi;
pub mod dl;
mod params_docs;

// Cross-process systems need a shared futex (Linux, macOS 14.4+); on other
// targets the module reduces to a no-op `worker_entry` and `build()` rejects
// process registrations. See docs/process-systems.md for the platform floor.
pub mod proc;

pub use dynamic::{FrameList, FrameMap};
pub use frame::Frame;
pub use text::FrameStr;
pub use writer::{DynamicWriteError, FrameWriter, KeyError, ListWriter, MapWriter};

pub use metor_fsw_ring::{ReadError, WriteError};

pub use binder::{AnySource, BindPorts, Binder, BoundInput, BoundPort, RingSource};
// `system` the fn (value namespace) coexists with `system` the attribute
// macro (macro namespace) at the root: `system(nav_execute)` and
// `#[metor_fsw_2::system]` resolve independently.
pub use coordinator::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorConfig, InitialOccupant, NAME_CAP,
    OccupantBacking, SlotConfigError, SlotState, SlotStatus, StopReason, StoppedSystem, WireError,
    WorkerRunState, WorkerStatus,
};
pub use handler::{
    AsyncSystemFn, BindCx, CycleCx, DeclSink, ExecParam, ExecParamSet, ExecuteFn, InitFn,
    IntoOutcome, IntoPackEntry, Params, SystemDef, TaskParam, system,
};
pub use pack::{
    Created, Driver, EntryParams, MakeError, Mount, Pack, PackEntry, Pending, StepStatus,
};
pub use registry::{AllOutputs, Registry, RegistryEntry};
pub use shared::{Shared, SharedLifecycle};
pub use telemetry::{
    DownlinkParams, LinkParams, LinkState, LinkStats, TelemetrySystem, UplinkParams, UplinkSystem,
};

pub use descriptor::{
    Capability, Declarations, Delivery, FanIn, Hz, PortConn, PortDesc, PortId, PortSchema,
    SystemDescriptor, SystemKind,
};
pub use health::{HealthPort, LogEvent, LogLevel, MAX_ERR_KINDS, MAX_LINES, SystemHealth};
pub use port::{
    DEFAULT_DEPTH, FrameRef, FrameWriteError, Input, Output, buffer_capacity, capacity_for,
};

pub use alarm::{AlarmIn, AlarmOut, AlarmSpec, AlarmSystem, AlarmsParams, BandSpec, TargetSpec};

pub use preset::{PresetOut, PresetSpec, PresetSystem, PresetsParams};

pub use message::{
    CommandOut, MAX_MSG_BYTES, MsgFanOut, MsgIn, MsgOut, MsgTable, NamedMsg, split_record,
};
pub use system::{
    AsyncContext, AsyncSystem, BuildCtx, BuildSystem, ConfigureError, CyclicRunner, CyclicSystem,
    HealthOutput, Out, System, SystemInput, SystemOutput,
};
#[doc(hidden)]
pub use system::{NoParamsDefault, ParamsDefaultProbe};

#[doc(hidden)]
pub use params_docs::ParamsDocEntry;

// Re-exported so `#[derive(inventory-based ParamsDocs)]` can name the crate.
#[doc(hidden)]
pub use inventory;

pub use metor_fsw_ring as ring;

pub use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_fsw_2_macros::{Frame, ParamsDocs, SystemInput, SystemOutput};

pub use metor_fsw_2_macros::system;

pub use metor_proto::types::Timestamp;

pub use sequence::{Outcome, SequenceStatus, SlotControlIn};

pub use metor_component::path;
pub use {metor_proto, metor_proto_wkt, zerocopy};

pub use dl::{DlError, DlPack, DlSystem};

pub use ir::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, DOWNLINK_TYPE, DistRef, EdgeSpec,
    InitialOccupantSpec, ParamSource, SlotInitState, SlotSpec, StateSpec, SystemSpec,
    TCP_SERVER_TYPE, UPLINK_TYPE, Wiring,
};

pub use wiring::{BuildError, BuildOptions, BundleError, PackageOptions, WiringBuilder};

pub mod cli;

// Frame acceptance tests span frame/dynamic/writer, so they live at the
// crate root rather than under any single module.
#[cfg(test)]
mod tests;
