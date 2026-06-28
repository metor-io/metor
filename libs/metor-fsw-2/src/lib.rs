//! Frames & component derives for the `metor-fsw-2` flight-software framework
//! (Work-Package 3).
//!
//! A [`Frame`] is a `#[repr(C)]` struct that, with one `#[derive(Frame)]`, becomes a
//! timestamped, [`ComponentId`](metor_proto::types::ComponentId)-named group of
//! components serializing straight to a metor-proto table — and that can carry
//! runtime-dynamic [`FrameList`]/[`FrameMap`] members. See `docs/frames.md`.
//!
//! This crate builds entirely on the landed WP1/WP2 primitives (the vtable
//! `List`/`Map`/`PathComponent`/`Frame` ops, `PathHasher`, the ring's record
//! alignment); the genuinely new surface is the [`Frame`] trait, the dynamic types,
//! and the [`FrameWriter`] producer API.

mod binder;
mod coordinator;
mod descriptor;
mod dynamic;
mod frame;
mod health;
mod port;
mod reader;
mod system;
mod writer;

// WP6 — the KDL wiring front-end (registry, FromKdlNode derive, `load()`).
#[cfg(feature = "kdl")]
pub mod wiring;

pub use dynamic::{FrameList, FrameMap, Name, Slot};
pub use frame::Frame;
pub use reader::{ListReader, MapReader};
pub use writer::{FrameWriter, ListWriter, MapWriter, WriteError};

// WP5 — the coordinator: builder, graph wiring, the run-phase lifecycle, and the
// `bind`/`Binder` port-construction contract.
pub use binder::{BindPorts, Binder, BoundPort};
pub use coordinator::{
    Coordinator, CoordinatorBuilder, CoordinatorConfig, PortRef, SlotState, StopReason,
    StoppedSystem, SystemHandle, WireError,
};

// WP4 — the system trait family, the typed port wrappers, self-description, and
// the standard health/log telemetry.
pub use descriptor::{Hz, PortDesc, SystemDescriptor, SystemKind, compatible};
pub use health::{
    HealthPort, LOG_MSG_CAP, Level, LogLine, MAX_ERR_KINDS, MAX_LINES, SystemHealth, SystemLog,
};
pub use port::{DEFAULT_DEPTH, FrameRef, Input, Output, buffer_capacity, capacity_for};
pub use system::{AsyncSystem, CyclicRunner, CyclicSystem, Out, System, SystemInput, SystemOutput};

// Re-export the ring transport so a system author only needs `metor_fsw_2::*`.
pub use metor_fsw_ring as ring;

// The derives (re-exported through metor-fsw) and the four component traits, so a
// user only needs `metor_fsw_2::*`.
pub use metor_fsw::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_fsw_macros::{Frame, SystemInput, SystemOutput};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_coordinator;
#[cfg(test)]
mod tests_system;
#[cfg(all(test, feature = "kdl"))]
mod tests_wiring;
