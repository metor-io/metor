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

mod dynamic;
mod frame;
mod reader;
mod writer;

pub use dynamic::{FrameList, FrameMap, Name, Slot};
pub use frame::Frame;
pub use reader::{ListReader, MapReader};
pub use writer::{FrameWriter, ListWriter, MapWriter, WriteError};

// The derives (re-exported through metor-fsw) and the four component traits, so a
// user only needs `metor_fsw_2::*`.
pub use metor_fsw::{AsVTable, Componentize, Decomponentize, Metadatatize};
pub use metor_fsw_macros::Frame;

#[cfg(test)]
mod tests;
