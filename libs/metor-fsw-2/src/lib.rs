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
mod registry;
mod system;
mod telemetry;
mod writer;

// WP6 — the KDL wiring front-end (registry, FromKdlNode derive, `load()`).
#[cfg(feature = "kdl")]
pub mod wiring;

// WP8 — the `dlopen` C-ABI a system `cdylib` exports (dl-open.md §2/§3). Rides the
// `kdl` feature in v1: `fsw_create` leans on WP6's `RegisteredSystem` construction
// contract (`src/wiring.rs`), so the ABI compiles wherever that does.
#[cfg(feature = "kdl")]
pub mod abi;

// WP8 Wave 2 — the host-side `dlopen` loader (`DlSystem`) + the cyclic slot (`DlSlot`)
// that drives a system `.so` across the ABI (dl-open.md §4). Rides the `kdl` feature
// with the `abi` module it consumes.
#[cfg(feature = "kdl")]
pub mod dl;

pub use dynamic::{FrameList, FrameMap, Name, Slot};
pub use frame::Frame;
pub use reader::{ListReader, MapReader};
pub use writer::{FrameWriter, ListWriter, MapWriter, WriteError};

// WP5 — the coordinator: builder, graph wiring, the run-phase lifecycle, and the
// `bind`/`Binder` port-construction contract.
pub use binder::{BindPorts, Binder, BoundPort, RingSource};
pub use coordinator::{
    ClockMode, Coordinator, CoordinatorBuilder, CoordinatorConfig, PortRef, SlotState, StopReason,
    StoppedSystem, SystemHandle, WireError,
};

// WP7 — the general output registry and the telemetry downlink (its first consumer).
pub use registry::{OutputRegistry, RegistryEntry};
pub use telemetry::{
    TcpTransport, TelemetryConfig, TelemetryMode, TelemetrySystem, Transport, TransportError,
};

// WP4 — the system trait family, the typed port wrappers, self-description, and
// the standard health/log telemetry.
pub use descriptor::{AnnounceFn, Hz, PortDesc, SystemDescriptor, SystemKind, compatible};
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

// WP8 — the `export_system!` macro that emits a system `cdylib`'s C-ABI surface.
pub use metor_fsw_macros::export_system;

// Re-exports the `#[derive(Frame)]` / `#[derive(FromKdlNode)]` generated code names
// (`::metor_fsw_2::metor_proto::…`, `::metor_fsw_2::path::…`, `::metor_fsw_2::kdl::…`)
// so a crate depending only on metor-fsw-2 can use those derives without a direct
// `metor-fsw` / `metor-proto` / `kdl` dependency.
pub use metor_fsw::path;
pub use {metor_proto, metor_proto_wkt};

// WP8 Wave 2 — the host dlopen loader surface.
#[cfg(feature = "kdl")]
pub use dl::{DlError, DlSystem};

#[cfg(feature = "kdl")]
pub use kdl;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_coordinator;
#[cfg(test)]
mod tests_system;
#[cfg(test)]
mod tests_telemetry;
#[cfg(all(test, feature = "kdl"))]
mod tests_wiring;
#[cfg(all(test, feature = "kdl"))]
mod tests_abi;
