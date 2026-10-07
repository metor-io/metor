//! IO ops, their completions, the buffers they use, and the backends that run
//! them.
//!
//! Ops are plain bytes. They refer to buffers by slot and to handles by index,
//! so they can be logged, compared, and copied across the guest ABI.

mod addr;
mod buf;
mod handle;
mod op;
#[cfg(all(feature = "host", unix))]
mod os;
mod out;
#[cfg(all(feature = "host", unix))]
mod poll;
#[cfg(all(feature = "host", unix))]
mod reactor;
#[cfg(all(feature = "host", target_os = "linux"))]
mod uring;

#[cfg(any(test, kani))]
mod tests;

pub use addr::RawAddr;
pub use buf::{BufError, BufRef, BufTable, RawBuf, TableId, Tables};
pub use handle::{RawHandle, Token};
pub use op::{GpioOp, Op, OpKind, OpenFlags, Sqe, TtyConfig, TtyOp};
#[cfg(all(feature = "host", unix))]
pub use os::{FdClass, Os, OsError, host};
pub use out::{Cqe, Errno, IoError, IoStatus, Out};
#[cfg(all(feature = "host", unix))]
pub use poll::PollOs;
#[cfg(all(feature = "host", unix))]
pub use reactor::{Limits, Reactor, SourceId};
#[cfg(all(feature = "host", target_os = "linux"))]
pub use uring::UringOs;
