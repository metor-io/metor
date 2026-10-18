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

pub use config::{
    Adapter, BuildOptions, Clock, CoordinatorConfig, DEFAULT_RING_DEPTH, InputConfig,
    InvalidWallRate, Location, OutputConfig, OutputSource, PortRef, SystemConfig, WallRate,
};
pub use error::BuildError;
pub use params::{MakeError, ParamError, Params};
#[cfg(feature = "host")]
pub use run::Coordinator;
pub use status::SystemStatus;
pub use step::{EndReason, LoadError, Occupant, Step};
pub use table::{MakeCx, SystemKind, SystemTable};
