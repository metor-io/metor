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
pub(crate) use config::Adapters;
pub use config::{
    Adapter, BuildOptions, ChannelConfig, ChannelName, ChannelNameTooLong, Clock,
    CoordinatorConfig, DEFAULT_RING_DEPTH, InputConfig, InvalidWallRate, Location, OutputConfig,
    OutputSource, PortRef, RunState, SystemConfig, WallRate,
};
pub use error::BuildError;
pub use params::{MakeError, ParamError, Params};
#[cfg(feature = "host")]
pub use run::Coordinator;
#[cfg(feature = "host")]
pub(crate) use status::STATUS_PORT;
pub use status::SystemStatus;
pub(crate) use step::StepState;
#[cfg(feature = "host")]
pub(crate) use step::catch_execute;
pub use step::{EndReason, LoadError, Occupant, Step, StepCx};
#[cfg(feature = "host")]
pub(crate) use table::MakeFn;
pub(crate) use table::TableEntry;
pub use table::{MakeCx, SystemKind, SystemTable};
