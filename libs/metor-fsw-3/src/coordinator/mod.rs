//! The coordinator steps systems in order over the rings that connect them.
//!
//! [`build`] resolves a config into systems and rings; [`run`] steps them.

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
pub(crate) use build::status_def;
pub use config::{
    Adapter, ChannelConfig, Clock, CoordinatorConfig, DEFAULT_RING_DEPTH, InputConfig,
    OutputConfig, OutputSource, PeerLink, PeerPort, PortRef, RunState, SystemConfig,
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
pub use table::{MakeCx, SystemTable};
pub(crate) use table::{TableEntry, bindings, refuse_thread};
