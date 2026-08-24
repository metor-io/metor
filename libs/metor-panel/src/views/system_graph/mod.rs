//! The native half of the graph: a live FSW's wiring, as nodes and wires.
//!
//! Topology comes from the [`WiringStore`](crate::wiring::WiringStore) — the
//! latest [`WiringManifest`](metor_proto_wkt::WiringManifest) the control
//! system broadcast — so what is drawn always reflects the live target. None
//! of it is editable: a native system's source of truth is Rust and
//! `target.py`, not a panel.
//!
//! What lives here is everything that is *about* wiring rather than about
//! drawing: [`layout`] turns a `Wiring` into deterministic node positions and
//! routes, [`config`] is the view state a layout persists, and
//! [`inspector_rows`] is what a selected node shows. The tile that used to own
//! them is [`crate::canvas`], which draws this beside the Python half because
//! the two were always one picture.

pub mod config;
pub mod inspector_rows;
pub mod layout;

#[cfg(test)]
mod tests;

pub use config::SystemGraphConfig;
