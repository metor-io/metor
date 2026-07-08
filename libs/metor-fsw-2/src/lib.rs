//! A framework for building modular flight software.
//!
//! A mission is a graph of **systems** (an IMU driver, a navigation filter, an
//! attitude controller) that a single **coordinator** wires together, schedules,
//! and observes. Systems exchange **frames** of typed components over
//! shared-memory ring buffers, and the graph streams its state off-board through
//! a downlink that is itself an ordinary system.
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
//! # Frames
//!
//! A [`Frame`] is a `#[repr(C)]` struct whose fields are components sharing one
//! logical timestamp, the whole group named by a `ComponentId`. One
//! `#[derive(Frame)]` propagates the marked timestamp field to every component
//! and describes the struct with a vtable, so a frame's in-memory bytes are also
//! its wire bytes. Nothing in the data path serializes; a peer system, the
//! downlink, and a ground database all read the same representation.
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
//! Everything derive-generated code names is re-exported from this crate's
//! root, so a system crate depends only on this crate and not on the component
//! and protocol crates underneath.
//!
//! [`FrameList`] and [`FrameMap`] members add bounded runtime dynamism (a
//! variable number of elements stored past the fixed region) while keeping the
//! worst-case frame size a compile-time constant, so rings can still be sized up
//! front. [`FrameWriter`] builds such a frame's bytes; [`ListReader`] and
//! [`MapReader`] read them back.
//!
//! # Systems and ports
//!
//! A system implements [`System`] plus one of two driving traits. A
//! [`CyclicSystem`] is ticked by the coordinator, which calls `execute` once per
//! cycle with the shared cycle [`Timestamp`]. An [`AsyncSystem`] owns its own
//! loop; the coordinator spawns `run` once and the system paces itself. The
//! `#[system]` attribute macro derives the port bundles and trait impls from an
//! inherent impl block, so most systems write only their `execute` or `run`.
//!
//! Ports are typed handles over ring buffers. An [`Output`] owns the ring a
//! frame is published to; an [`Input`] borrows a read-only view of an upstream
//! ring. Publishing a fixed-size frame is a single ring write, and reading hands
//! back a zero-copy grant borrowed in place. A writer never overwrites a record
//! a reader has not consumed; a full ring surfaces as a [`WriteError`] instead
//! of silent loss. Beside frames there is a second payload kind, postcard
//! **messages** ([`MsgOut`], [`MsgIn`]), for commands and events that fan in
//! from many producers.
//!
//! Systems never return errors. Every output bundle carries an implicit
//! health/log port pair (the [`health`] module), and the framework publishes
//! per-system [`SystemHealth`] and [`SystemLog`] frames each cycle, so a
//! system's troubles flow off-board like any other telemetry.
//!
//! # The coordinator and wiring
//!
//! The [`Coordinator`] reads each system's static [`SystemDescriptor`] before
//! constructing anything, validates the connection graph, allocates and sizes
//! every ring, binds the ports, and then drives the cycle on a wall or simulated
//! clock. Graphs are wired in Rust through [`CoordinatorBuilder`], or
//! declaratively in KDL through the [`wiring`] module (the `kdl` feature).
//!
//! A system can also be compiled as a `cdylib` exporting the C ABI in [`abi`]
//! (via [`export_system!`] or `#[system(export)]`) and loaded at runtime through
//! [`dl`]. A loaded system describes itself over the ABI and is validated and
//! wired exactly like a statically linked one. Both modules build without the
//! `kdl` feature.
//!
//! # Built-in systems
//!
//! The crate ships the common infrastructure as ordinary systems: the TCP
//! telemetry downlink and command uplink ([`TelemetrySystem`],
//! [`UplinkSystem`]), the limit-alarm engine ([`AlarmSystem`]), and the
//! [`sequence`] runtime that turns a `#[sequence]` async fn into a loadable
//! occupant of a coordinator slot.
//!
//! The `docs/` directory of this crate holds the detailed design documents;
//! `DESIGN.md` is the overview.

mod alarm;
mod binder;
mod coordinator;
mod descriptor;
mod dynamic;
mod frame;
mod message;
mod port;
mod reader;
mod registry;
mod system;
mod telemetry;
mod writer;

pub mod health;

// Not gated on `kdl`; sequences are an ABI/runtime feature.
pub mod sequence;

#[cfg(feature = "kdl")]
pub mod wiring;

pub mod abi;
pub mod dl;

// Cross-process systems need a shared futex (Linux, macOS 14.4+); on other
// targets the module reduces to a no-op `worker_entry` and `build()` rejects
// process registrations. See docs/process-systems.md §2 for the floor.
pub mod proc;

pub use dynamic::{FrameList, FrameMap, Slot};
pub use frame::Frame;
pub use reader::{ListReader, MapReader};
pub use writer::{FrameWriter, KeyError, ListWriter, MapWriter};

pub use metor_fsw_ring::{ReadError, WriteError};

pub use binder::{BindPorts, Binder, BoundInput, BoundPort, RingSource};
pub use coordinator::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorBuilder, CoordinatorConfig,
    InitialOccupant, NAME_CAP, PortRef, SLOT_NAME_CAP, SlotState, SlotStatus, StopReason,
    StoppedSystem, SystemHandle, WireError,
};

pub use registry::{AllOutputs, EntrySchema, Registry, RegistryEntry};
pub use telemetry::{
    DownlinkParams, RecvTransport, TcpRecvTransport, TcpTransport, TelemetryConfig, TelemetryMode,
    TelemetrySystem, Transport, TransportError, UplinkParams, UplinkSystem,
};

pub use descriptor::{
    AnnounceFn, Capability, Delivery, FanIn, Hz, PortConn, PortDecl, PortDesc, PortId, PortSchema,
    SystemDescriptor, SystemKind, split_decls,
};
pub use health::{
    HealthPort, LOG_MSG_CAP, LogLine, MAX_ERR_KINDS, MAX_LINES, SystemHealth, SystemLog,
};
pub use port::{DEFAULT_DEPTH, FrameRef, Input, Output, buffer_capacity, capacity_for};

pub use alarm::{AlarmIn, AlarmOut, AlarmSpec, AlarmSystem, AlarmsParams, BandSpec, TargetSpec};

pub use message::{
    CommandOut, MAX_MSG_BYTES, MsgFanOut, MsgIn, MsgOut, MsgTable, NamedMsg, split_record,
};
pub use system::{
    AsyncSystem, BuildCtx, BuildSystem, ConfigureError, CyclicRunner, CyclicSystem, Out, System,
    SystemInput, SystemOutput,
};

pub use metor_fsw_ring as ring;

pub use metor_fsw::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_fsw_2_macros::{Frame, SystemInput, SystemOutput};

pub use metor_fsw_2_macros::export_system;

pub use metor_fsw_2_macros::sequence;

pub use metor_fsw_2_macros::system;

pub use metor_proto::types::Timestamp;

pub use sequence::{
    Outcome, Seq, SeqBound, SeqClock, SeqStatusOut, SeqSystem, SequenceStatus, SlotControlIn,
};

pub use metor_fsw::path;
pub use {metor_proto, metor_proto_wkt};

pub use dl::{DlError, DlSystem};

#[cfg(feature = "kdl")]
pub use wiring::{
    AllowedOccupantSpec, Artifact, BuildError, BuildOptions, BundleError, ClockSpec,
    CoordinatorSpec, EdgeSpec, InitialOccupantSpec, PackageOptions, ParamSource, SlotInitState,
    SlotSpec, SystemSpec, TCP_DOWNLINK_TYPE, TCP_UPLINK_TYPE, Wiring, WiringBuilder,
};

#[cfg(feature = "kdl")]
pub mod cli;

#[cfg(feature = "kdl")]
pub use kdl;

// Frame acceptance tests span frame/dynamic/writer/reader, so they live at the
// crate root rather than under any single module.
#[cfg(test)]
mod tests;
