//! What runs ops against the host: the `Os` backends, and the dispatch
//! between every system's queue and them.

use core::ptr::NonNull;
use std::os::fd::{OwnedFd, RawFd};

use super::{IoError, Op, Token};

pub(crate) mod dispatch;
pub(crate) use dispatch::{Backed, Dispatch, Route};

/// How a descriptor's ops run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Stream,
    Datagram,
    Listener,
    Tty,
    File,
    Dir,
    Gpio,
}

impl Class {
    /// Whether ops wait for readiness instead of blocking.
    pub fn polled(self) -> bool {
        matches!(
            self,
            Self::Stream | Self::Datagram | Self::Listener | Self::Tty
        )
    }
}

/// An op's index in the dispatch's op table, and its system's generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(u32);

impl OpId {
    pub const fn new(index: u16, generation: u16) -> Self {
        Self(index as u32 | (generation as u32) << 16)
    }

    pub fn index(self) -> u16 {
        self.0 as u16
    }

    pub fn generation(self) -> u16 {
        (self.0 >> 16) as u16
    }

    pub fn get(self) -> u32 {
        self.0
    }

    pub fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The id as a token, for a sim op's request and reply.
    pub fn token(self) -> Token {
        Token::new(self.index(), self.generation())
    }
}

/// An op ready for an `Os`: its descriptor and buffer resolved.
#[derive(Debug)]
pub struct OsOp {
    pub id: OpId,
    pub fd: RawFd,
    pub class: Class,
    pub op: Op,
    pub buf: Option<NonNull<[u8]>>,
    /// The arena `buf` lies in, for a backend that registers arenas.
    pub arena: u16,
}

#[derive(Debug)]
pub enum Out {
    Len(u32),
    Opened(OwnedFd, Class),
}

#[derive(Debug)]
pub struct OsDone {
    pub id: OpId,
    pub result: Result<Out, IoError>,
}

/// Runs ops against the host. No method blocks.
pub trait Os {
    /// Queues an op. A refused op comes back from `reap` with its error.
    fn push(&mut self, op: OsOp);
    /// Cancels an op in flight. The op still completes, usually `Canceled`.
    fn cancel(&mut self, id: OpId);
    /// Hands queued ops to the kernel.
    fn submit(&mut self);
    /// Moves every finished op into `done`.
    fn reap(&mut self, done: &mut Vec<OsDone>);
    /// Closes a descriptor an op opened, canceling its ops in flight.
    fn close(&mut self, fd: OwnedFd);
}
