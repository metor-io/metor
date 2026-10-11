//! The coordinator runs systems in a fixed order, once per cycle. Systems pass
//! data to each other through rings: each output writes to its own ring, and
//! each input reads from the rings of the outputs it is wired to.
//!
//! The `build` module turns a config into systems and rings. The `run` module
//! steps them.

#[cfg(feature = "host")]
mod build;
mod config;
mod error;
mod params;
#[cfg(feature = "host")]
mod run;
mod status;
mod step;
mod table;

#[cfg(all(test, feature = "host"))]
mod tests;

#[cfg(feature = "host")]
pub(crate) use build::{instance_def, status_def};
pub use config::{
    Adapter, BuildOptions, ChannelConfig, Clock, CoordinatorConfig, DEFAULT_RING_DEPTH,
    InputConfig, InvalidWallRate, OutputConfig, OutputSource, PeerLink, PeerPort, PortRef,
    RunState, SystemConfig, WallRate,
};
pub use error::BuildError;
pub use params::{ParamError, Params};
#[cfg(feature = "host")]
pub use run::Coordinator;
pub use status::SystemStatus;
pub use step::{EndReason, LoadError, Occupant, Step};
pub(crate) use step::{Stepped, catch_step};
#[cfg(feature = "host")]
pub(crate) use table::MakeFn;
pub use table::{MakeCx, SystemKind, SystemTable};
pub(crate) use table::{TableEntry, bindings, refuse_thread};
