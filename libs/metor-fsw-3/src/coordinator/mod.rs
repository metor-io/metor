//! Builds a graph from a config and steps it.
//!
//! [`Coordinator::build`] resolves a [`CoordinatorConfig`] against a
//! [`SystemTable`], allocates one ring per output plus one `status` ring per
//! system, and binds every port. [`Coordinator::step`] then runs the systems in
//! list order; [`Coordinator::run`] paces that on the stellarator runtime.
//!
//! Every allocation happens in `build`. A cycle allocates nothing.

mod build;
mod config;
mod error;
#[cfg(test)]
mod fixtures;
mod run;
mod status;
mod table;

use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;

use crate::port::Output;

pub use config::{Clock, CoordinatorConfig, InputConfig, PortRef, SystemConfig};
pub use error::BuildError;
pub use run::Step;
pub use status::SystemStatus;
pub use table::SystemTable;

/// One bound system and the status port the coordinator owns for it.
struct Entry {
    name: String,
    step: Box<dyn Step>,
    status: Output<SystemStatus>,
}

/// A built graph: the systems in step order, the clock that stamps each cycle,
/// and the rings holding them together.
pub struct Coordinator {
    entries: Vec<Entry>,
    clock: Clock,
    /// Start of the simulated timeline, taken at build.
    epoch: Timestamp,
    cycle: u64,
    /// Declared last so every port drops before the region it points into.
    rings: Vec<RingBuffer>,
}

impl Coordinator {
    /// Rings allocated: one per output port plus one status ring per system.
    pub fn rings(&self) -> usize {
        self.rings.len()
    }

    /// The systems' config ids, in step order.
    pub fn entry_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.name.as_str())
    }
}
