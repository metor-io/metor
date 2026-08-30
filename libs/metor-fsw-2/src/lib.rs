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
//! reporting that loss on the log.
//!
//! Each system has a log output; cyclic drivers flush its queued [`LogEvent`]
//! messages after each step. The coordinator publishes each system's
//! [`SystemStatus`] run record itself; a free-running [`AsyncSystem`]
//! publishes its own through its context.
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
mod async_system;
mod coordinator;
mod io_bridge;
mod preset;
mod telemetry;

// The pure-data target IR; the `wiring` module re-exports these types
// alongside its resolver.
pub mod ir;

pub mod wiring;

pub mod dl;
pub mod wasm;

// Cross-process systems need a shared futex (Linux, macOS 14.4+); on other
// targets the module reduces to a no-op `worker_entry` and `build()` rejects
// process registrations. See docs/process-systems.md for the platform floor.
pub mod proc;

pub mod cli;

// The authoring surface, whole. A target crate depends on this crate and sees
// one framework; a pack, sequence, or contract crate depends on
// `metor-fsw-2-core` alone and sees the half that compiles for wasm.
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

// Fn-authored systems and pack entries end to end, through a live
// coordinator — hence here rather than beside the handler in the core crate.
#[cfg(test)]
mod tests;
