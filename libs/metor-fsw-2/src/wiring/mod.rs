//! Load a coordinator from a [`Wiring`] mission IR.
//!
//! This module is a pure front-end onto the init graph, no coordinator logic of
//! its own. The pipeline is one line:
//!
//! ```text
//! WiringBuilder ─┐
//!                ├─→ Wiring ─→ validate ─→ resolve ─→ InitGraph ─→ Coordinator
//!  mission.py ───┘
//! ```
//!
//! Both front-ends — the Rust [`WiringBuilder`] and an evaluated `.py` mission
//! ([`eval_python_mission`]) — produce the same [`Wiring`], a plain data model
//! of systems, slots, edges, and coordinator config. [`resolve`](resolve::resolve)
//! ([`resolve.rs`](resolve)) gates it through [`validate`](validate), then walks
//! it: a [`Node`](crate::coordinator::init::Node) per system (static systems via
//! the [`Registry`] in [`registry.rs`](registry), dl and process systems from
//! their built [`Artifact`]s), the edges, and the coordinator config, all pushed
//! onto an [`InitGraph`](crate::coordinator::init::InitGraph) and built
//! ([`InitGraph::build`](crate::coordinator::init::InitGraph::build)). One resolver feeds both
//! front-ends, so every graph check runs identically for either. Every error is
//! a [`LoadError`], a `miette` [`Diagnostic`](miette::Diagnostic) anchored on the
//! spec's [`SourceRef`] when it has one.
//!
//! Around that core sit the supporting surfaces: [`bundle`] packs a resolved
//! mission and its artifacts into a `.metor` archive; [`build_driver`] compiles
//! an [`Artifact`]'s cdylib; [`stubgen`] generates the Python stub module for a
//! built pack; [`params`] encodes a value tree against an artifact's exported
//! `Params` schema.
//!
//! ## Params
//!
//! Params reach `resolve` as a [`ParamSource`] — a value tree
//! ([`ParamSource::Value`], the Python front-end's format), the Rust builder's
//! pre-encoded postcard bytes ([`ParamSource::Postcard`]), or nothing
//! ([`ParamSource::None`]). A static system deserializes a typed `S::Params`
//! (serde, field defaults honored). A loaded system's `Params` type is never
//! linked into the host, so a value tree is conformed against the artifact's
//! exported `Params` schema and encoded to postcard bytes
//! ([`encode_value_params`]); pre-encoded bytes pass straight through.

// The `Wiring` data model's two front-ends (the Python-eval path and the Rust
// builder), the registry and resolver split below, and the supporting bundle,
// build-driver, and stubgen surfaces.
mod build_driver;
mod builder;
mod bundle;
mod error;
mod params;
mod py;
mod registry;
mod resolve;
mod stubgen;
mod validate;

// The pure-data IR lives at the crate root (feature `wiring-model`) so it is
// available without the front-end; the submodules here reach it as
// `super::model`.
pub(crate) use crate::ir as model;

pub use build_driver::{BuildError, BuildOptions, build_artifacts, build_target};
pub use builder::{ArtifactSystemBuilder, SlotSpecBuilder, SystemSpecBuilder, WiringBuilder};
pub use bundle::{
    BundleError, BundleMeta, METOR_EXTENSION, PackageOptions, WIRING_FILE_NAME, load_bundle,
    unpack_metor, write_bundle,
};
pub use error::{Anchor, LoadError, LoadErrorKind};
pub use model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec, IR_VERSION,
    InitialOccupantSpec, ParamSource, ScopeSpec, SlotInitState, SlotSpec, SourceRef, SystemSpec,
    TCP_DOWNLINK_TYPE, TCP_UPLINK_TYPE, Wiring,
};
pub use params::encode_value_params;
pub use py::{eval_python_mission, is_python_mission};
pub use registry::{AsyncKind, CyclicKind, IntoNode, Registry};
pub use resolve::{ResolveOptions, resolve, resolve_with};
pub use stubgen::{StubgenError, StubgenOptions, StubgenReport, stubgen};

// The typed value-tree param decode, shared with the `pack` bind path.
pub(crate) use registry::decode_value_params;

/// The shared-object file name for a library `stem` on the host platform
/// (`libfoo.so`, `libfoo.dylib`, `foo.dll`), used by the build driver and the
/// stub generator to locate an [`Artifact::lib`]'s output.
pub fn cdylib_file_name(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// The shared-object file name for a library `stem` on a `triple`, the
/// arch-parameterized sibling of [`cdylib_file_name`]. The IR carries the bare
/// stem ([`Artifact::lib`]); this is where a target triple turns it into a
/// file name — at provision, bundle, and `pack dev` time.
pub fn cdylib_file_name_for(triple: &str, stem: &str) -> String {
    if triple.contains("-apple-") {
        format!("lib{stem}.dylib")
    } else if triple.contains("-windows-") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

// Re-exported so a system author only needs `metor_fsw_2::wiring`.
pub use crate::register_system;

#[cfg(test)]
mod tests;
