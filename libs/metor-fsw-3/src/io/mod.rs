//! IO ops, their completions, the buffers they use, and the backends that run
//! them.
//!
//! Ops are plain bytes. They refer to buffers by slot and to handles by index,
//! so they can be logged, compared, and copied across the guest ABI.

use std::borrow::Cow;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod addr;
mod buf;
mod cap;
pub(crate) mod device;
mod handle;
mod op;
#[cfg(all(feature = "host", unix))]
mod os;
mod out;
mod param;
mod pending;
#[cfg(all(feature = "host", unix))]
mod poll;
pub(crate) mod queue;
#[cfg(all(feature = "host", unix))]
mod reactor;
mod record;
mod slot;
#[cfg(all(feature = "host", target_os = "linux"))]
mod uring;

#[cfg(any(test, kani))]
pub(crate) mod tests;

pub use addr::RawAddr;
pub use buf::{BufError, BufRef, BufTable, RawBuf, TableId, Tables};
pub use cap::{
    Can, Cap, Dial, Dir, File, GpioLine, Handle, Interface, Listen, Messages, Stream, TtyQueue,
    Uart, Udp,
};
pub use device::DeviceSide;
pub use handle::{RawHandle, Token};
pub use op::{GpioOp, Op, OpKind, OpenFlags, Sqe, TtyConfig, TtyOp};
#[cfg(all(feature = "host", unix))]
pub use os::{FdClass, Os, OsError, host};
pub use out::{Cqe, Errno, IoError, IoStatus, Out};
pub use pending::{
    Accept, BufOp, Close, Fsync, Gpio, IoOp, Open, Pending, Raw, Read, Recv, Send, Tty, Write,
};
#[cfg(all(feature = "host", unix))]
pub use poll::PollOs;
pub use queue::Io;
#[cfg(all(feature = "host", unix))]
pub use reactor::{Limits, Reactor, SourceId};
pub use slot::{Buf, IoInit, Slot};
#[cfg(all(feature = "host", target_os = "linux"))]
pub use uring::UringOs;

/// The output a system submits ops on.
pub const IO_SQ: &str = "io_sq";
/// The input a system reads completions from.
pub const IO_CQ: &str = "io_cq";
/// The most ops one system may have in flight, and the ring depth of `Sqe`
/// and `Cqe`.
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
