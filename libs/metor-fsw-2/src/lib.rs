//! Build flight software as a checked graph of small systems.
//!
//! The coordinator owns the graph, its ring buffers, and the cycle clock. Each
//! system reads typed inputs and writes typed outputs, and the wiring pass
//! checks the graph before the first cycle runs.
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
//! A [`Frame`] is a `#[repr(C)]` struct whose fields share one timestamp; its
//! memory bytes are also its ring and wire bytes, and a vtable describes those
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
//! [`FrameList`] and [`FrameMap`] add a bounded variable-size trailer, sized by
//! a const bound so the coordinator knows a ring's worst-case record size.
//! [`FrameWriter`] builds these records.
//!
//! Commands and events travel as postcard messages through [`MsgOut`] and
//! [`MsgIn`]. A message input may accept many producers; each producer still
//! owns its own single-writer ring.
//!
//! # Systems
//!
//! The crate has four authoring forms:
//!
//! - A function passed to [`system()`] runs once per cycle; its arguments
//!   declare its ports.
//! - A function passed to [`Pack::task`] owns its ports in a future that the
//!   driver polls once per cycle. Sequences use this form.
//! - A [`CyclicSystem`] struct runs once per cycle.
//! - An [`AsyncSystem`] struct owns a free-running task that waits on input or
//!   time; the coordinator never polls it directly.
//!
//! An [`Output`] owns a ring writer, and an [`Input`] owns a read view into an
//! upstream ring. A full ring turns a write into a [`WriteError`]; the
//! `publish` helpers keep the cycle moving by dropping the record and logging
//! the loss.
//!
//! Every system has a log output, and cyclic drivers flush its queued
//! [`LogEvent`] messages after each step. The coordinator publishes each
//! system's [`SystemStatus`] run record itself; a free-running [`AsyncSystem`]
//! publishes its own through its context.
//!
//! # Wiring and loading
//!
//! A `target.py` file and [`WiringBuilder`] both produce the same [`Wiring`]
//! IR. The resolver checks that IR, loads each system descriptor, builds the
//! graph, sizes its rings, and hands back a ready [`Coordinator`].
//!
//! A [`Pack`] lists the systems one crate exports. The host can link a pack
//! directly, load its `cdylib` through [`dl`], or run one of its entries in a
//! worker process through [`proc`]; all three paths share the same
//! descriptors and port rules. A pack crate depends only on
//! `metor-fsw-2-core`, the half of this framework that also compiles for
//! wasm; a target crate depends on this crate and gets the whole surface,
//! re-exported below.

mod alarm;
mod async_system;
mod coordinator;
mod io_bridge;
mod preset;
mod telemetry;

// The target's pure-data description; `wiring` builds and resolves it.
pub mod ir;

pub mod wiring;

pub mod dl;
pub mod wasm;

// Cross-process systems share a futex, available on Linux and macOS 14.4+;
// other targets get a no-op `worker_entry`, and `build()` rejects process
// registrations there.
pub mod proc;

pub mod cli;

pub use metor_fsw_2_core::*;

pub use async_system::{AsyncContext, AsyncSystem};
pub use coordinator::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorConfig, InitialOccupant, OccupantBacking,
    SlotConfigError, SlotStatus, WireError,
};
pub use telemetry::{
    DownlinkParams, LinkParams, LinkState, LinkStats, TelemetrySystem, UplinkParams, UplinkSystem,
};

pub use alarm::{AlarmIn, AlarmOut, AlarmSpec, AlarmSystem, AlarmsParams, BandSpec, TargetSpec};

pub use preset::{PresetOut, PresetSpec, PresetSystem, PresetsParams};

pub use dl::{DlError, DlPack, DlSystem};

pub use ir::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, DOWNLINK_TYPE, DistRef, EdgeSpec,
    InitialOccupantSpec, ParamSource, ProgramDecl, ProgramSpec, SlotInitState, SlotSpec, StateSpec,
    SystemSpec, TCP_SERVER_TYPE, UPLINK_TYPE, Wiring,
};

pub use wiring::{BuildError, BuildOptions, BundleError, PackageOptions, WiringBuilder};

// Fn-authored systems and pack entries run end to end here, against a live
// coordinator, rather than beside their handlers in the core crate.
#[cfg(test)]
mod tests;
