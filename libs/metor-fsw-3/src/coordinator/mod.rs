//! The coordinator steps systems in order over the rings that connect them.
//!
//! [`build`] resolves a config into systems and rings; [`run`] steps them.

mod build;
mod config;
mod error;
mod params;
mod run;
mod status;
mod step;
mod table;

#[cfg(test)]
mod tests;

use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;

use crate::port::Output;

pub(crate) use build::status_def;
pub use config::{
    Adapter, ChannelConfig, Clock, CoordinatorConfig, DEFAULT_RING_DEPTH, InputConfig,
    OutputConfig, OutputSource, PeerLink, PeerPort, PortRef, RunState, SystemConfig,
};
pub use error::BuildError;
pub use params::{ParamError, Params};
pub(crate) use run::{Stepped, catch_step};
pub use status::SystemStatus;
pub use step::{EndReason, LoadError, Occupant, Step};
pub use table::{MakeCx, SystemTable};
pub(crate) use table::{MakeFn, TableEntry, bindings, refuse_thread};

/// One bound system and the status port the coordinator owns for it.
struct Entry {
    name: String,
    step: Option<Box<dyn Step>>,
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
    /// Retained handles share ownership of these regions.
    rings: Vec<RingBuffer>,
}

impl Coordinator {
    /// Rings allocated: one per output port plus one status ring per system.
    pub fn rings(&self) -> usize {
        self.rings.len()
    }

    /// The ids of the systems that ended and how, in step order.
    pub fn ended(&self) -> impl Iterator<Item = (&str, &EndReason)> {
        self.entries
            .iter()
            .filter_map(|entry| Some((entry.name.as_str(), entry.ended()?)))
    }

    /// The ids of the systems a panic latched off, in step order.
    pub fn latched(&self) -> impl Iterator<Item = &str> {
        self.ended()
            .filter(|(_, end)| **end == EndReason::Panicked)
            .map(|(id, _)| id)
    }
}
