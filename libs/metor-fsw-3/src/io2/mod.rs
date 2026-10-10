//! IO as a queue: a system puts ops on its own submission queue and takes
//! results from its own completion queue.
//!
//! Every index is arithmetic over tables sized at build. A buffer is a
//! [`Span`] of its system's arena, so ops are plain bytes that can be logged,
//! compared, and copied across the guest ABI.
#![allow(dead_code, unused_imports)]

use std::borrow::Cow;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod arena;
mod cap;
mod config;
mod device;
mod error;
#[cfg(all(feature = "host", unix))]
pub mod host;
mod queue;
mod status;
pub mod test;
mod wire;

#[cfg(test)]
mod tests;

pub use arena::{Buf, IoInit};
pub use cap::{
    Can, Cap, Dir, File, GpioLine, Handle, Interface, Listen, Messages, Stream, Uart, Udp,
};
pub use config::{Access, Backing, CanFilter, GpioDirection, RealKind, ResourceConfig, SimKind};
pub use device::{DeviceSide, GpioModel, Model, Reply, UartModel};
pub use error::{Errno, IoError, IoStatus};
pub use queue::{
    Accept, BufOp, Close, Fsync, Gpio, Io, IoOp, Open, Pending, Raw, Read, Recv, Send, Slot, Tty,
    Write,
};
pub use status::ResourceStatus;
pub use wire::{
    Cqe, Op, OpKind, OpenFlags, RawAddr, RawHandle, Span, Sqe, Token, TtyConfig, TtyQueue,
};

/// The output a system submits ops on.
pub const IO_SQ: &str = "io_sq";
/// The input a system reads completions from.
pub const IO_CQ: &str = "io_cq";
/// The most ops one system may have in flight.
#[cfg(not(kani))]
pub const MAX_OPS: usize = 16;
#[cfg(kani)]
pub const MAX_OPS: usize = 4;

/// A config entry that gives a system's `Cap<K>` parameter a resource.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Grant {
    pub param: String,
    pub resource: String,
}

/// A `Cap<K>` or `DeviceSide<K>` parameter in a system's definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantDef {
    pub param: Cow<'static, str>,
    /// The interface's name, such as `"uart"`.
    pub interface: Cow<'static, str>,
    pub end: End,
}

/// Which end of a resource a parameter holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum End {
    /// A `Cap<K>`: the end a driver submits ops on.
    Host,
    /// A `DeviceSide<K>`: the end a model system serves.
    Device,
}
