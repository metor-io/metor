//! What runs ops against the host: the `Os` backends, and the dispatch
//! between every system's queue and them.

use core::net::SocketAddr;
use core::ptr::NonNull;
use std::collections::HashSet;
use std::os::fd::{OwnedFd, RawFd};

use indexmap::IndexMap;

use super::{Backing, Cqe, Generational, IoState, ResourceIndex, Sqe};
use crate::coordinator::BuildError;
use crate::port::{Input, Output};
use dispatch::Resource;
use metor_fsw_3_ring::{NoWake, RingBuffer};
use wire::Wire;

pub(crate) mod dispatch;
mod open;
mod poll;
mod steps;
mod sys;
#[cfg(target_os = "linux")]
mod uring;
pub(crate) mod wire;

pub(crate) use dispatch::{Backed, Dispatch, MemberDef, Route};
pub use open::{OpenError, open, resolve};
pub use poll::PollOs;
pub(crate) use steps::entry;
pub use steps::{IO_STATUS_PORT, IO_TYPE};
#[cfg(target_os = "linux")]
pub use uring::UringOs;

pub use super::{
    Access, Baud, Errno, IoError, Op, OpKind, OpenFlags, RealKind, TtyConfig, TtyQueue,
};

/// How a descriptor's ops run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Stream,
    Datagram,
    Listener,
    /// No descriptor until an op dials one.
    Dialer,
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
            Self::Stream | Self::Datagram | Self::Listener | Self::Dialer | Self::Tty
        )
    }
}

/// The table an [`OpId`] indexes.
pub enum DispatchTable {}

/// An op's index in the dispatch's op table, and its system's generation.
pub type OpId = Generational<DispatchTable>;

/// An op ready for an `Os`: its descriptor and buffer resolved.
#[derive(Clone, Copy, Debug)]
pub struct OsOp {
    pub id: OpId,
    /// `-1` for a `Dialer`, which has none.
    pub fd: RawFd,
    pub class: Class,
    pub op: Op,
    pub buf: Option<NonNull<[u8]>>,
    /// Where a `Dialer`'s `Stream` connects.
    pub dial: Option<SocketAddr>,
}

impl OsOp {
    /// The op's buffer, or an empty one.
    pub(crate) fn bytes(&self) -> NonNull<[u8]> {
        let empty = NonNull::slice_from_raw_parts(NonNull::dangling(), 0);
        self.buf.unwrap_or(empty)
    }

    /// Whether the op waits for its descriptor to take writes: a write, or
    /// a dial's connect.
    pub(crate) fn waits_to_write(&self) -> bool {
        self.op.writes() || self.class == Class::Dialer
    }
}

#[derive(Debug)]
pub enum Out {
    Len(u32),
    /// A new descriptor, and for a stream its peer.
    Opened {
        fd: OwnedFd,
        class: Class,
        peer: Option<SocketAddr>,
    },
}

impl Out {
    /// A length of `n` bytes, which an op's buffer bounds below `u32::MAX`.
    pub(crate) fn len(n: usize) -> Self {
        Self::Len(n as u32)
    }
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

/// The dispatch, the backend its ops run on, and each member's queue pair.
pub(crate) struct Host {
    pub(crate) dispatch: Dispatch,
    pub(crate) os: Box<dyn Os>,
    /// Each member's SQ and CQ, by member index, once an io step binds them.
    pub(crate) queues: Box<[MemberQueue]>,
}

/// A member's SQ, which an io step lowers, and its CQ, which one posts to.
///
/// They outlive the io steps, so a remade step reads on where the last one
/// stopped.
#[derive(Default)]
pub(crate) struct MemberQueue {
    pub(crate) sq: Option<Input<Sqe>>,
    pub(crate) cq: Option<Output<Cqe>>,
}

impl Host {
    /// Binds the SQ of each `(member, rings)` that has none yet.
    pub(crate) fn bind_sqs<'a>(&mut self, sqs: impl Iterator<Item = (usize, Vec<&'a RingBuffer>)>) {
        for (m, rings) in sqs {
            let Some(MemberQueue { sq: sq @ None, .. }) = self.queues.get_mut(m) else {
                continue;
            };
            // PANIC Safety: the build pass counts one reader slot per edge,
            // and a bound SQ keeps its view across remakes.
            let views = rings
                .into_iter()
                .map(|ring| ring.view(NoWake).expect("a counted reader slot"));
            *sq = Some(Input::new(views.collect()));
        }
    }

    /// Binds the CQ of each `(member, ring)` that has none yet.
    pub(crate) fn bind_cqs<'a>(&mut self, cqs: impl Iterator<Item = (usize, &'a RingBuffer)>) {
        for (m, ring) in cqs {
            let Some(MemberQueue { cq: cq @ None, .. }) = self.queues.get_mut(m) else {
                continue;
            };
            // PANIC Safety: build allocates one ring per output, and a bound
            // CQ keeps its writer across remakes.
            *cq = Some(Output::new(
                ring.writer(NoWake).expect("one writer per output ring"),
            ));
        }
    }

    /// Cancels every op and reaps for a few rounds. False if any op is still
    /// in flight.
    pub(crate) fn teardown(&mut self) -> bool {
        let Self { dispatch, os, .. } = self;
        dispatch.quiesce_all();
        let mut done = Vec::new();
        for _ in 0..TEARDOWN_ROUNDS {
            let routes: Vec<Route> = dispatch.drain_routes().collect();
            for route in routes {
                match route {
                    Route::Cancel(id) => os.cancel(id),
                    Route::Close(fd) => os.close(fd),
                    Route::Os(op) => dispatch.raise(OsDone {
                        id: op.id,
                        result: Err(IoError::Canceled),
                    }),
                }
            }
            os.submit();
            os.reap(&mut done);
            for d in done.drain(..) {
                dispatch.raise(d);
            }
            if !dispatch.has_kernel_ops_in_flight() {
                return true;
            }
            std::thread::sleep(core::time::Duration::from_millis(1));
        }
        false
    }
}

impl IoState {
    /// A target's IO: one member per system with a `Cap`, given as its id and
    /// def, in config order. Opens every real resource a member holds, makes
    /// a wire for each sim one, and the backend.
    pub(crate) fn build(
        resources: &IndexMap<String, Backing>,
        members: Vec<(String, MemberDef)>,
    ) -> Result<Self, BuildError> {
        if members.is_empty() {
            return Ok(Self::default());
        }
        let ops = Dispatch::check_ops(members.iter().map(|(_, m)| usize::from(m.ops)).sum())?;
        let held: HashSet<ResourceIndex> = members
            .iter()
            .flat_map(|(_, m)| m.grants.iter().map(|&(res, _)| res))
            .collect();
        let mut wires = Vec::new();
        let resources = resources
            .iter()
            .enumerate()
            .map(|(index, (id, backing))| {
                let held = ResourceIndex::try_from(index).is_ok_and(|r| held.contains(&r));
                Resource::open(id, backing, held, ops, &mut wires)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut io = Self::with_members(members.iter().map(|(id, _)| id.as_str()));
        let queues = members.iter().map(|_| MemberQueue::default()).collect();
        let defs = members.into_iter().map(|(_, def)| def).collect();
        let dispatch = Dispatch::new(defs, resources, wires)?;
        let os = host(ops).map_err(|e| BuildError::IoBackend {
            message: e.to_string(),
        })?;
        io.host = Some(Host {
            dispatch,
            os,
            queues,
        });
        Ok(io)
    }
}

impl Resource {
    /// Resource `id`, opened if it is `held`, or wired by adding its wire to
    /// `wires`. A wire parks at most `ops` ops per end.
    fn open(
        id: &str,
        backing: &Backing,
        held: bool,
        ops: usize,
        wires: &mut Vec<Wire>,
    ) -> Result<Self, BuildError> {
        let failed = |e: OpenError| BuildError::Open {
            resource: id.to_string(),
            message: e.to_string(),
        };
        let backed = match backing {
            Backing::Sim(sim) if held => {
                wires.push(Wire::new(sim.kind, ops));
                Backed::Wire(wires.len() - 1)
            }
            Backing::Real(RealKind::TcpDial { addr }) if held => {
                Backed::Dial(resolve(addr).map_err(failed)?)
            }
            Backing::Real(kind @ RealKind::TcpListen { allow, .. }) if held => {
                let (fd, _) = open(kind).map_err(failed)?;
                Backed::Listen(fd, allow.as_slice().into())
            }
            Backing::Real(kind) if held => {
                let (fd, class) = open(kind).map_err(failed)?;
                Backed::Fd(fd, class)
            }
            _ => Backed::Unused,
        };
        Ok(Self {
            id: id.to_string(),
            backed,
        })
    }
}
