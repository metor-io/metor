//! The op types systems submit and the completions they read back.
//!
//! An op is plain bytes: it names buffers by slot and handles by index, never
//! by pointer, so it can be logged, compared, and copied across the guest ABI.

mod addr;
mod buf;
mod handle;
mod op;
#[cfg(all(feature = "host", unix))]
mod os;
mod out;
#[cfg(all(feature = "host", unix))]
mod reactor;

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
pub use reactor::{Limits, Reactor, SourceId};
