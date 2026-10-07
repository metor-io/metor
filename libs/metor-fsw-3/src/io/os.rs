use std::os::fd::OwnedFd;

use super::{Cqe, Limits, RawAddr, RawHandle, Sqe, Tables};

/// What a registered descriptor is. It picks the path each op takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdClass {
    Stream,
    Datagram,
    Listener,
    File,
    Dir,
    Tty,
    Can,
    Gpio,
}

/// Executes ops against the host. Nothing here blocks.
pub trait Os {
    /// Queues an op. A full queue comes back from `reap` as `QueueFull`.
    fn push(&mut self, sqe: Sqe);
    /// Hands queued ops to the kernel.
    fn submit(&mut self);
    /// Takes every completion that has landed.
    fn reap(&mut self, done: &mut Vec<Cqe>);
    /// Takes ownership of an opened resource; ops name the handle it returns.
    fn register(&mut self, fd: OwnedFd, class: FdClass) -> RawHandle;
}

#[derive(Debug, thiserror::Error)]
pub enum OsError {
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The host backend.
pub fn host(_limits: &Limits, _tables: Tables, _dials: &[RawAddr]) -> Result<Box<dyn Os>, OsError> {
    Err(OsError::Unsupported("no host backend"))
}
