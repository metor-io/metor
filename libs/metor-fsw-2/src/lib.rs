//! Frames & component derives for the `metor-fsw-2` flight-software framework.
//!
//! A [`Frame`] is a `#[repr(C)]` struct that, with one `#[derive(Frame)]`, becomes a
//! timestamped, [`ComponentId`](metor_proto::types::ComponentId)-named group of
//! components serializing straight to a metor-proto table — and that can carry
//! runtime-dynamic [`FrameList`]/[`FrameMap`] members. See `docs/frames.md`.
//!
//! This crate builds on the metor-proto primitives (the vtable
//! `List`/`Map`/`PathComponent`/`Frame` ops, `PathHasher`, the ring's record
//! alignment); its own surface is the [`Frame`] trait, the dynamic types,
//! and the [`FrameWriter`] producer API.

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

// Public: `health::Level` is deliberately namespaced (a bare `Level` at the root
// is collision-prone — S3); the frame types themselves stay root re-exports.
pub mod health;

// The sequence-occupant runtime (the future-driven state machine a `#[sequence]`
// async fn becomes). Ungated alongside `abi`/`dl` — sequences are an ABI/runtime
// feature, not KDL.
pub mod sequence;

// The KDL wiring front-end (registry, the KDL params deserializer, `load()`).
#[cfg(feature = "kdl")]
pub mod wiring;

// The `dlopen` C-ABI a system `cdylib` exports. Construction is decoupled from KDL (the
// `BuildSystem` contract in `src/system.rs`), so the ABI is available with or without
// the `kdl` feature: `fsw_create` leans only on that kdl-independent contract.
pub mod abi;

// The host-side `dlopen` loader (`DlSystem`) + the cyclic slot (`DlSlot`) that drives a
// system `.so` across the ABI. Ungated alongside `abi`.
pub mod dl;

pub use dynamic::{FrameList, FrameMap, Slot};
pub use frame::Frame;
pub use reader::{ListReader, MapReader};
pub use writer::{FrameWriter, KeyError, ListWriter, MapWriter};

// The transport-level errors the port APIs actually return (`Output::write` /
// `MsgOut::emit` → `WriteError`; `Input::drain`/`recv` → `ReadError`) — re-exported
// at the root so `?`-ing them needs no `metor_fsw_ring` path (S2).
pub use metor_fsw_ring::{ReadError, WriteError};

// The coordinator: builder, graph wiring, the run-phase lifecycle, and the
// `bind`/`Binder` port-construction contract.
pub use binder::{BindPorts, Binder, BoundInput, BoundPort, RingSource};
pub use coordinator::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorBuilder, CoordinatorConfig,
    InitialOccupant, NAME_CAP, PortRef, SLOT_NAME_CAP, SlotState, SlotStatus,
    StopReason, StoppedSystem, SystemHandle, WireError,
};

// The general output registry and the telemetry downlink (its first consumer) + the uplink
// command-plane ingest system (its read twin).
pub use registry::{AllOutputs, EntrySchema, Registry, RegistryEntry};
pub use telemetry::{
    DownlinkParams, RecvTransport, TcpRecvTransport, TcpTransport, TelemetryConfig,
    TelemetryMode, TelemetrySystem, Transport, TransportError, UplinkParams, UplinkSystem,
};

// The system trait family, the typed port wrappers, self-description, and the
// standard health/log telemetry.
// `compatible`/`split_decls` stay off the root: generic free-function names are
// collision-prone under a glob import (S3). Wiring resolves compatibility itself;
// the derives call `split_decls` through `crate::` paths.
pub use descriptor::{
    AnnounceFn, Capability, Delivery, FanIn, Hz, PortConn, PortDecl, PortDesc, PortId,
    PortSchema, SystemDescriptor, SystemKind, split_decls,
};
// `health::Level` stays namespaced (S3) — write `health::Level::Warn`.
pub use health::{
    HealthPort, LOG_MSG_CAP, LogLine, MAX_ERR_KINDS, MAX_LINES, SystemHealth, SystemLog,
};
pub use port::{DEFAULT_DEPTH, FrameRef, Input, Output, buffer_capacity, capacity_for};

// The alarm engine: config surface + the limit-alarm evaluation system
// (`docs/alarms.md`).
pub use alarm::{AlarmIn, AlarmOut, AlarmSpec, AlarmSystem, AlarmsParams, BandSpec, TargetSpec};

// The general message channel — the `(PacketId, postcard)` record format, the
// type-erased emit port, and its sizing/record helpers (`docs/messages.md` §1, §2).
pub use message::{
    CommandOut, MAX_MSG_BYTES, MsgFanOut, MsgIn, MsgOut, MsgTable, NamedMsg, split_record,
};
pub use system::{
    AsyncSystem, BuildCtx, BuildSystem, ConfigureError, CyclicRunner, CyclicSystem, Out, System,
    SystemInput, SystemOutput,
};

// Re-export the ring transport so a system author only needs `metor_fsw_2::*`.
pub use metor_fsw_ring as ring;

// The four component traits (defined by metor-fsw) and the fsw-2 derives (owned by
// the metor-fsw-2-macros crate), so a user only needs `metor_fsw_2::*`.
pub use metor_fsw::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_fsw_2_macros::{Frame, SystemInput, SystemOutput};

// The `export_system!` macro that emits a system `cdylib`'s C-ABI surface (the
// hand-written escape hatch; `#[system(export)]` emits the same surface).
pub use metor_fsw_2_macros::export_system;

// The `#[sequence]` attribute macro that turns an `async fn` into a dl-loadable
// occupant (the sequence twin of `export_system!`). Coexists with the `sequence`
// module above (module in the type namespace, macro in the macro namespace).
pub use metor_fsw_2_macros::sequence;

// The `#[system]` attribute macro: annotates a system's inherent impl block and
// derives the port bundles, `System` + `CyclicSystem`/`AsyncSystem`, `BuildSystem`,
// and (opt-in) the `fsw_*` exports from the `execute`/`run` signature
// (`docs/design-system-macro.md`).
pub use metor_fsw_2_macros::system;

// The coordinator cycle timestamp every `execute` receives — re-exported at the root
// so `#[system]`-authored code writes `now: Timestamp` without reaching into
// `metor_proto::types` (`docs/design-system-macro.md` §3.1).
pub use metor_proto::types::Timestamp;

// The sequence-occupant runtime surface: the **types** the macro/seam name. The free
// author API (`wait`/`progress`/`aborted`/`now`) and the generic-named `Step`/`Wait`
// stay namespaced under [`sequence`] (S3) — `metor_fsw_2::wait()` or a bare `Wait`
// at the root would be collision-prone, and `metor_fsw_2::now()` would read as wall
// time (review E7).
pub use sequence::{
    Outcome, Seq, SeqBound, SeqClock, SeqStatusOut, SeqSystem, SequenceStatus, SlotControlIn,
};

// Re-exports the `#[derive(Frame)]` generated code names
// (`::metor_fsw_2::metor_proto::…`, `::metor_fsw_2::path::…`, `::metor_fsw_2::kdl::…`)
// so a crate depending only on metor-fsw-2 can use those derives without a direct
// `metor-fsw` / `metor-proto` / `kdl` dependency.
pub use metor_fsw::path;
pub use {metor_proto, metor_proto_wkt};

// The host dlopen loader surface (available without `kdl`).
pub use dl::{DlError, DlSystem};

// The `Wiring` data model, the Rust builder, the shared resolver, and the cargo build
// driver (the data/serialization split). These ride the `kdl` feature with the wiring
// front-end they share a resolver with.
// Types only (S3): the wiring free functions (`wiring::parse`, `wiring::resolve`,
// `wiring::load`, `wiring::build_artifacts`, `wiring::load_bundle`, …) stay
// namespaced — `metor_fsw_2::parse`/`resolve` at the root are collision-prone
// under the glob imports the docs recommend.
#[cfg(feature = "kdl")]
pub use wiring::{
    AllowedOccupantSpec, Artifact, BuildError, BuildOptions, BundleError, ClockSpec,
    CoordinatorSpec, EdgeSpec, InitialOccupantSpec, PackageOptions, ParamSource, SlotInitState,
    SlotSpec, SystemSpec, TCP_DOWNLINK_TYPE, TCP_UPLINK_TYPE, Wiring, WiringBuilder,
};

// The clap-based CLI runner (`metor-fsw {build,package,run}`) and the reusable
// `cli::main()`/`cli::run()` entry points. Rides the `kdl` feature (it uses
// `parse`/`build_artifacts`/`resolve`); the `metor-fsw` bin delegates to `cli::main()`.
#[cfg(feature = "kdl")]
pub mod cli;

#[cfg(feature = "kdl")]
pub use kdl;

// Frame acceptance tests span frame/dynamic/writer/reader, so they stay a
// crate-root tests module rather than living under any single module.
#[cfg(test)]
mod tests;
