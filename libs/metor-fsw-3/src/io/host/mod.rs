//! What runs ops against the host: the `Os` backends, and the dispatch
//! between every system's queue and them.

use core::ptr::NonNull;
use std::os::fd::{OwnedFd, RawFd};

use super::{Backing, IoState, MAX_OPS, ResourceConfig, ResourceEnd, Token};
use crate::coordinator::BuildError;
use dispatch::Resource;
use wire::Wire;

pub(crate) mod dispatch;
mod open;
mod poll;
mod steps;
mod sys;
#[cfg(target_os = "linux")]
mod uring;
pub(crate) mod wire;

pub(crate) use dispatch::{Backed, Dispatch, Route};
pub use open::{OpenError, open};
pub use poll::PollOs;
pub(crate) use steps::entry;
pub use steps::{IO_STATUS_PORT, IO_TYPE};
#[cfg(target_os = "linux")]
pub use uring::UringOs;

pub use super::{Access, Errno, IoError, Op, OpKind, OpenFlags, RealKind, TtyConfig, TtyQueue};

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
}

impl OsOp {
    /// The op's buffer, or an empty one.
    pub(crate) fn bytes(&self) -> NonNull<[u8]> {
        let empty = NonNull::slice_from_raw_parts(NonNull::dangling(), 0);
        self.buf.unwrap_or(empty)
    }
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

/// The host's backend for at most `ops` ops in flight: `UringOs` on Linux,
/// `PollOs` elsewhere.
pub fn host(ops: usize) -> std::io::Result<Box<dyn Os>> {
    #[cfg(target_os = "linux")]
    return Ok(Box::new(UringOs::new(ops)?));
    #[cfg(not(target_os = "linux"))]
    return Ok(Box::new(PollOs::new(ops)?));
}

/// How many submit and reap rounds teardown runs before it gives up.
const TEARDOWN_ROUNDS: usize = 10;

/// The dispatch and the backend its ops run on.
pub(crate) struct Host {
    pub(crate) dispatch: Dispatch,
    pub(crate) os: Box<dyn Os>,
}

impl Host {
    /// Cancels every op and reaps for a few rounds. False if any op is still
    /// in flight.
    pub(crate) fn teardown(&mut self) -> bool {
        let Self { dispatch, os } = self;
        dispatch.quiesce_all();
        let mut done = Vec::new();
        for _ in 0..TEARDOWN_ROUNDS {
            for route in dispatch.drain_routes() {
                match route {
                    Route::Cancel(id) => os.cancel(id),
                    Route::Close(fd) => os.close(fd),
                    Route::Os(_) => {}
                }
            }
            os.submit();
            os.reap(&mut done);
            for d in done.drain(..) {
                dispatch.raise(d);
            }
            if !dispatch.live() {
                return true;
            }
            std::thread::sleep(core::time::Duration::from_millis(1));
        }
        false
    }
}

/// A target's IO: one member per system with a `Cap`, given as its id and
/// the resource and end of each grant, in config order. Opens every real
/// resource a member holds, makes a wire for each sim one, and the backend.
pub(crate) fn build(
    resources: &[ResourceConfig],
    members: Vec<(String, Vec<(u16, ResourceEnd)>)>,
) -> Result<IoState, BuildError> {
    if members.is_empty() {
        return Ok(IoState::default());
    }
    let ops = members.len() * MAX_OPS;
    let held = |at: usize| {
        let mut grants = members.iter().flat_map(|(_, g)| g);
        grants.any(|&(r, _)| usize::from(r) == at)
    };
    let mut wires = Vec::new();
    let resources = (0..resources.len())
        .map(|at| resource(&resources[at], held(at), ops, &mut wires))
        .collect::<Result<Vec<_>, _>>()?;
    let grants = members.iter().map(|(_, g)| g.clone()).collect();
    let dispatch = Dispatch::new(grants, resources, wires)?;
    let os = host(ops).map_err(|e| BuildError::IoSystems {
        message: format!("the IO backend: {e}"),
    })?;
    let mut io = IoState::with_members(members.iter().map(|(id, _)| id.as_str()));
    io.host = Some(Host { dispatch, os });
    Ok(io)
}

/// A resource, opened if it is `held`, or wired by adding its wire to
/// `wires`. A wire parks at most `ops` ops per end.
fn resource(
    config: &ResourceConfig,
    held: bool,
    ops: usize,
    wires: &mut Vec<Wire>,
) -> Result<Resource, BuildError> {
    let backed = match &config.backing {
        Backing::Sim(kind) if held => {
            wires.push(Wire::new(*kind, ops));
            Backed::Wire(wires.len() - 1)
        }
        Backing::Real(kind) if held => {
            let (fd, class) = open(kind).map_err(|e| BuildError::Open {
                resource: config.id.clone(),
                message: e.to_string(),
            })?;
            Backed::Fd(fd, class)
        }
        _ => Backed::Unused,
    };
    Ok(Resource {
        id: config.id.clone(),
        backed,
    })
}
