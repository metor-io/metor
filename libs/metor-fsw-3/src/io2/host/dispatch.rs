//! `Dispatch`: every system's handles and ops in flight, in tables sized at
//! build.
//!
//! The op system `m` submits in slot `s` is entry `m * MAX_OPS + s`, so a
//! completion finds its system and token by arithmetic. `lower` turns an
//! `Sqe` into a `Route` for the io steps to carry out, and `raise` turns a
//! finished op back into a `Cqe`. Nothing here does IO.

use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

use metor_component::FrameStr;
use metor_proto::types::Timestamp;

use super::{Class, OpId, OsDone, OsOp, Out};
use crate::coordinator::ParamError;
use crate::io2::arena::Arena;
use crate::io2::status::{Counters, ResourceStatus};
use crate::io2::{Cqe, IoError, MAX_OPS, Op, OpKind, RawHandle, Span, Sqe, Token};
use crate::port::Output;

/// The most `io_status` records written in one cycle.
const STATUS_PER_CYCLE: usize = 16;

/// What backs a resource.
pub(crate) enum Backed {
    Fd(OwnedFd, Class),
    /// A sim resource, by its model's port index.
    Sim(u16),
    /// No system holds it, so it was never opened.
    Unused,
}

pub(crate) struct Resource {
    pub(crate) name: FrameStr<64>,
    pub(crate) backed: Backed,
}

/// Something for the io steps to carry out.
#[derive(Debug)]
pub(crate) enum Route {
    Os(OsOp),
    Cancel(OpId),
    /// A request for the model on sim port `.0`.
    Sim(u16, Sqe),
    Close(OwnedFd),
}

enum Open {
    Closed,
    Grant,
    Derived(OwnedFd, Class),
}

struct Handle {
    generation: u16,
    res: u16,
    open: Open,
}

struct Member {
    arena: Rc<Arena>,
    handles: Box<[Handle]>,
    grants: usize,
    quiescing: bool,
    drained: bool,
}

#[derive(Clone, Copy)]
struct Entry {
    generation: u16,
    res: u16,
    kind: OpKind,
    live: bool,
    canceled: bool,
}

/// The descriptor or sim port an op on a handle goes to.
enum Target {
    Fd(i32, Class),
    Sim(u16),
}

pub(crate) struct Dispatch {
    members: Box<[Member]>,
    ops: Box<[Entry]>,
    resources: Box<[Resource]>,
    counters: Box<[Counters]>,
    reported: Box<[Counters]>,
    next_status: usize,
    routes: Vec<Route>,
    replies: Vec<(usize, Cqe)>,
}

impl Dispatch {
    /// One member per system with a `Cap`: its arena and the resource of each
    /// grant, in order.
    pub(crate) fn new(
        members: Vec<(Rc<Arena>, Vec<u16>)>,
        resources: Vec<Resource>,
    ) -> Result<Self, ParamError> {
        let ops = members.len() * MAX_OPS;
        if ops > usize::from(u16::MAX) + 1 {
            return Err(ParamError::Resource {
                id: "io".into(),
                message: format!("{} systems hold a `Cap`, past the limit", members.len()),
            });
        }
        let idle = Entry {
            generation: 0,
            res: 0,
            kind: OpKind::Close,
            live: false,
            canceled: false,
        };
        let count = resources.len();
        Ok(Self {
            members: members.into_iter().map(member).collect(),
            ops: vec![idle; ops].into(),
            resources: resources.into(),
            counters: vec![Counters::default(); count].into(),
            reported: vec![Counters::default(); count].into(),
            next_status: 0,
            routes: Vec::with_capacity(3 * ops),
            replies: Vec::with_capacity(2 * ops),
        })
    }

    /// Lowers one op from member `m`'s queue.
    pub(crate) fn lower(&mut self, m: usize, sqe: Sqe) {
        if self.members[m].quiescing {
            return;
        }
        if let Err(e) = self.try_lower(m, sqe) {
            self.replies.push((m, Cqe::err(sqe.token, e)));
        }
    }

    fn try_lower(&mut self, m: usize, sqe: Sqe) -> Result<(), IoError> {
        let op = sqe.op()?;
        if op == Op::Cancel {
            self.cancel(m, sqe.token);
            return Ok(());
        }
        let (res, target) = self.target(m, sqe.handle).ok_or(IoError::Stale)?;
        if op == Op::Close {
            self.close(m, sqe.handle);
            self.replies.push((m, Cqe::ok(sqe.token, 0)));
            return Ok(());
        }
        let buf = match op.fills() || op.drains() {
            true => Some(self.members[m].arena.span(sqe.buf).ok_or(IoError::BadBuf)?),
            false => None,
        };
        let index = index(m, sqe.token.slot()).ok_or(IoError::Refused)?;
        let entry = &mut self.ops[index];
        if entry.live {
            return Err(IoError::Refused);
        }
        *entry = Entry {
            generation: sqe.token.generation(),
            res,
            kind: op.kind(),
            live: true,
            canceled: false,
        };
        self.counters[usize::from(res)].submitted += 1;
        let id = OpId::new(index as u16, sqe.token.generation());
        self.routes.push(match target {
            Target::Fd(fd, class) => Route::Os(OsOp {
                id,
                fd,
                class,
                op,
                buf,
            }),
            Target::Sim(port) => {
                let mut req = sqe;
                req.token = id.token();
                Route::Sim(port, req)
            }
        });
        Ok(())
    }

    /// The live handle `raw` names in member `m`, with its resource.
    fn target(&self, m: usize, raw: RawHandle) -> Option<(u16, Target)> {
        let handle = self.members[m].handles.get(usize::from(raw.index()))?;
        if handle.generation != raw.generation() {
            return None;
        }
        let target = match &handle.open {
            Open::Closed => return None,
            Open::Derived(fd, class) => Target::Fd(fd.as_raw_fd(), *class),
            Open::Grant => match &self.resources[usize::from(handle.res)].backed {
                Backed::Fd(fd, class) => Target::Fd(fd.as_raw_fd(), *class),
                Backed::Sim(port) => Target::Sim(*port),
                Backed::Unused => return None,
            },
        };
        Some((handle.res, target))
    }

    /// Ends a handle. A grant ends only this capability; a derived handle's
    /// descriptor closes.
    fn close(&mut self, m: usize, raw: RawHandle) {
        let handle = &mut self.members[m].handles[usize::from(raw.index())];
        if let Open::Derived(fd, _) = core::mem::replace(&mut handle.open, Open::Closed) {
            self.routes.push(Route::Close(fd));
        }
    }

    /// Cancels member `m`'s op `token`, once.
    fn cancel(&mut self, m: usize, token: Token) {
        let Some(index) = index(m, token.slot()) else {
            return;
        };
        let entry = &mut self.ops[index];
        if !entry.live || entry.canceled || entry.generation != token.generation() {
            return;
        }
        entry.canceled = true;
        let id = OpId::new(index as u16, entry.generation);
        self.routes
            .push(match self.resources[usize::from(entry.res)].backed {
                Backed::Sim(port) => {
                    let none = RawHandle::new(0, 0);
                    Route::Sim(
                        port,
                        Sqe::new(id.token(), none, &Op::Cancel, Span::default()),
                    )
                }
                _ => Route::Cancel(id),
            });
    }

    /// The completion of a finished op, for its member's CQ. `None` when no
    /// system waits for it.
    pub(crate) fn raise(&mut self, done: OsDone) -> Option<(usize, Cqe)> {
        let index = usize::from(done.id.index());
        let entry = self.ops.get_mut(index)?;
        if !entry.live || entry.generation != done.id.generation() {
            return None;
        }
        entry.live = false;
        let entry = *entry;
        let m = index / MAX_OPS;
        let token = Token::new((index % MAX_OPS) as u16, entry.generation);
        let result = match done.result {
            Ok(Out::Len(n)) => Ok(n),
            Ok(Out::Opened(fd, class)) => self.derive(m, entry.res, fd, class),
            Err(e) => Err(e),
        };
        if self.members[m].quiescing {
            return None;
        }
        count(
            &mut self.counters[usize::from(entry.res)],
            entry.kind,
            &result,
        );
        Some((
            m,
            result.map_or_else(|e| Cqe::err(token, e), |n| Cqe::ok(token, n)),
        ))
    }

    /// Raises a model's reply.
    pub(crate) fn raise_sim(&mut self, cqe: Cqe) -> Option<(usize, Cqe)> {
        let id = OpId::new(cqe.token.slot(), cqe.token.generation());
        self.raise(OsDone {
            id,
            result: cqe.result().map(Out::Len),
        })
    }

    /// Maps a new descriptor to a free derived handle of member `m`, or closes
    /// it when none is free or the member is quiescing.
    fn derive(&mut self, m: usize, res: u16, fd: OwnedFd, class: Class) -> Result<u32, IoError> {
        let member = &mut self.members[m];
        let free = member.handles[member.grants..]
            .iter()
            .position(|h| matches!(h.open, Open::Closed));
        let (Some(at), false) = (free, member.quiescing) else {
            self.routes.push(Route::Close(fd));
            return Err(IoError::Refused);
        };
        let at = member.grants + at;
        let handle = &mut member.handles[at];
        handle.generation = handle.generation.wrapping_add(1);
        handle.res = res;
        handle.open = Open::Derived(fd, class);
        Ok(RawHandle::new(at as u16, handle.generation).get())
    }

    /// Starts quiescing member `m`: drops its new ops and cancels those in
    /// flight.
    pub(crate) fn begin(&mut self, m: usize) {
        let member = &mut self.members[m];
        member.quiescing = true;
        member.drained = false;
        for slot in 0..MAX_OPS {
            let entry = self.ops[m * MAX_OPS + slot];
            self.cancel(m, Token::new(slot as u16, entry.generation));
        }
    }

    /// Notes that a quiescing member's queue is drained. The first time,
    /// closes its derived handles and revives its grants for the next make.
    pub(crate) fn drained(&mut self, m: usize) {
        let member = &mut self.members[m];
        if !member.quiescing || core::mem::replace(&mut member.drained, true) {
            return;
        }
        let grants = member.grants;
        for (at, handle) in member.handles.iter_mut().enumerate() {
            let open = match at < grants {
                true => Open::Grant,
                false => Open::Closed,
            };
            if let Open::Derived(fd, _) = core::mem::replace(&mut handle.open, open) {
                self.routes.push(Route::Close(fd));
            }
        }
    }

    pub(crate) fn quiescing(&self, m: usize) -> bool {
        self.members[m].quiescing
    }

    /// Whether member `m` has nothing left in flight since `begin`.
    pub(crate) fn idle(&self, m: usize) -> bool {
        let member = &self.members[m];
        let ops = &self.ops[m * MAX_OPS..(m + 1) * MAX_OPS];
        !member.quiescing || (member.drained && ops.iter().all(|e| !e.live))
    }

    pub(crate) fn end(&mut self, m: usize) {
        self.members[m].quiescing = false;
    }

    /// The routes queued since the last call.
    pub(crate) fn routes(&mut self) -> std::vec::Drain<'_, Route> {
        self.routes.drain(..)
    }

    /// Queues a route again, such as a sim cancel its port refused.
    pub(crate) fn defer(&mut self, route: Route) {
        self.routes.push(route);
    }

    /// The ops answered without running, since the last call.
    pub(crate) fn replies(&mut self) -> std::vec::Drain<'_, (usize, Cqe)> {
        self.replies.drain(..)
    }

    /// Writes up to `STATUS_PER_CYCLE` changed resources, starting after the
    /// last one written. A record the ring refuses is written again later.
    pub(crate) fn report(&mut self, now: Timestamp, out: &Output<ResourceStatus>) {
        let count = self.counters.len();
        let mut written = 0;
        for step in 0..count {
            let at = (self.next_status + step) % count;
            if self.counters[at] == self.reported[at] {
                continue;
            }
            let status = self.counters[at].status(now, self.resources[at].name);
            if written == STATUS_PER_CYCLE || out.write(&status).is_err() {
                self.next_status = at;
                return;
            }
            self.reported[at] = self.counters[at];
            written += 1;
        }
    }
}

/// The op table index of member `m`'s slot, if the slot is one.
fn index(m: usize, slot: u16) -> Option<usize> {
    let slot = usize::from(slot);
    (slot < MAX_OPS).then_some(m * MAX_OPS + slot)
}

fn member((arena, grants): (Rc<Arena>, Vec<u16>)) -> Member {
    let grant = |&res: &u16| Handle {
        generation: 0,
        res,
        open: Open::Grant,
    };
    let free = (0..MAX_OPS).map(|_| Handle {
        generation: 0,
        res: 0,
        open: Open::Closed,
    });
    Member {
        arena,
        handles: grants.iter().map(grant).chain(free).collect(),
        grants: grants.len(),
        quiescing: false,
        drained: false,
    }
}

/// Counts one finished op: its result, and the bytes it moved.
fn count(counters: &mut Counters, kind: OpKind, result: &Result<u32, IoError>) {
    counters.completed += 1;
    let moved = match result {
        Ok(n) => u64::from(*n),
        Err(_) => {
            counters.failed += 1;
            0
        }
    };
    match kind {
        OpKind::Read | OpKind::Recv => counters.bytes_in += moved,
        OpKind::Write | OpKind::Send | OpKind::SendTo => counters.bytes_out += moved,
        _ => {}
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Distinct members' slots never share an entry, and an entry gives back
    /// its member and slot.
    #[kani::proof]
    fn index_never_aliases() {
        let (m, slot): (u16, u16) = (kani::any(), kani::any());
        kani::assume(usize::from(m) < (usize::from(u16::MAX) + 1) / MAX_OPS);
        let m = usize::from(m);
        if let Some(at) = index(m, slot) {
            assert_eq!((at / MAX_OPS, at % MAX_OPS), (m, usize::from(slot)));
            let id = OpId::new(at as u16, kani::any());
            assert_eq!(usize::from(id.index()), at);
        }
    }
}
