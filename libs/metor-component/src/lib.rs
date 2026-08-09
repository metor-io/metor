//! The component model the metor flight-software crates share.
//!
//! A *component* is one named, typed value on the wire. Components are
//! addressed by a dotted path (`adcs.wheels.0.speed`), and a struct becomes a
//! group of them by implementing four traits:
//!
//! - [`AsVTable`] describes the struct's `#[repr(C)]` bytes as a
//!   [`metor_proto`] vtable, so a reader can slice components straight out of
//!   a frame without copying.
//! - [`Metadatatize`] emits the human-facing [`ComponentMetadata`] — names,
//!   element labels, units — that goes with those components.
//! - [`Componentize`]/[`Decomponentize`] (re-exported from
//!   [`metor_proto::com_de`]) move component values in and out of a sink.
//!
//! Derive all four with `#[derive(AsVTable, Metadatatize, Componentize,
//! Decomponentize)]`. `metor-fsw-2` re-exports this whole surface and bundles
//! the same four derives into its `#[derive(Frame)]`, so framework consumers
//! rarely name this crate directly — the exception is a nested struct that is
//! a component group but not a frame in its own right.
//!
//! Paths are the load-bearing detail. [`ComponentPath`](path::ComponentPath)
//! composes prefixes without allocating, and hashes to the same
//! [`ComponentId`](metor_proto::types::ComponentId) the dotted string would,
//! so a component's identity is stable no matter how deeply it nests.
//!
//! [`ComponentMetadata`]: metor_proto_wkt::ComponentMetadata

pub use metor_component_macros::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_proto::com_de::{Componentize, Decomponentize};
pub use metadata::Metadatatize;
pub use vtable::AsVTable;
pub use {metor_proto, metor_proto_wkt, zerocopy};

pub mod metadata;
mod nox;
pub mod path;
mod vtable;
