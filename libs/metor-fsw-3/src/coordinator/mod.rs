//! The coordinator is the core part of metor-fsw, it is responsible for executing systems in order, and plumbing together ring buffers
//!
//! The meat and potatoes of coordinator is in [`build`] which is responsible for passing through the configuration, and connecting
//! all of the ring buffers together. Beyond that the coordinator is very simple, it just executes systems in order.
//!  [`run`] contains that code.

mod build;
mod config;
mod error;
mod params;
mod run;
mod status;
mod table;

use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;

use crate::port::Output;

pub use config::{Clock, CoordinatorConfig, InputConfig, PortRef, SystemConfig};
pub use error::BuildError;
pub use params::{ParamError, Params};
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
