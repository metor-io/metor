//! IO as a queue: a system puts ops on its own submission queue and takes
//! results from its own completion queue.
//!
//! Every index is arithmetic over tables sized at build. A buffer is a
//! [`Span`] of its system's arena, so ops are plain bytes that can be logged,
//! compared, and copied across the guest ABI.

use std::borrow::Cow;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod arena;
mod cap;
mod config;
mod error;
#[cfg(all(feature = "host", unix))]
pub mod host;
mod op_slot;
mod ops;
mod param;
mod queue;
mod shared;
mod status;
pub mod test;
mod wire;

#[cfg(test)]
pub(crate) mod tests;

pub use arena::*;
pub use cap::*;
pub use config::*;
pub use error::*;
#[cfg(all(feature = "host", unix))]
pub use host::{IO_STATUS_PORT, IO_TYPE};
pub use ops::*;
pub use queue::*;
pub use shared::*;
pub use status::*;
pub use wire::*;

/// The output a system submits ops on.
pub const IO_SQ: &str = "io_sq";
/// The input a system reads completions from.
pub const IO_CQ: &str = "io_cq";
/// The most ops one system may have in flight.
///
/// Kani proofs use a smaller queue to keep their state space tractable.
#[cfg(not(kani))]
pub const MAX_OPS: usize = 16;
#[cfg(kani)]
pub const MAX_OPS: usize = 4;

/// A `Cap<K>` parameter in a system's definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantDef {
    pub param: Cow<'static, str>,
    /// The interface's name, such as `"uart"`.
    pub interface: Cow<'static, str>,
}

impl GrantDef {
    /// The definition of a `Cap<K>` parameter called `param`.
    pub fn new<K: Interface>(param: &'static str) -> Self {
        Self {
            param: param.into(),
            interface: K::NAME.into(),
        }
    }
}

/// Which end of a resource a system holds.
///
/// A sim resource is a wire between a driver and a device model. A real
/// resource has only the driver's end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceEnd {
    /// The driver's end.
    #[default]
    Host,
    /// The device model's end.
    Device,
}

impl ResourceEnd {
    pub fn is_host(&self) -> bool {
        *self == Self::Host
    }

    /// The other end of the wire.
    pub fn other(self) -> Self {
        match self {
            Self::Host => Self::Device,
            Self::Device => Self::Host,
        }
    }
}
