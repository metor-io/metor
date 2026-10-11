//! `Dispatch`: every system's handles and ops in flight, in tables sized at
//! build.
//!
//! The op system `m` submits in slot `s` is entry `m * MAX_OPS + s`, so a
//! completion finds its system and token by arithmetic. `lower` turns an
//! `Sqe` into a `Route` for the io steps to carry out, and `raise` turns a
//! finished op back into a `Cqe`. Nothing here does IO.

use std::os::fd::{AsRawFd, OwnedFd};

use metor_component::FrameStr;
use metor_proto::types::Timestamp;

use super::wire::Wire;
use super::{Class, OpId, OsDone, OsOp, Out};
use crate::coordinator::BuildError;
use crate::io::arena::Arena;
use crate::io::status::{Counters, ResourceStatus, resource_name};
use crate::io::{Cqe, IoError, MAX_OPS, Op, OpKind, RawHandle, ResourceEnd, Sqe, Token};
use crate::port::Output;

/// The most `io_status` records written in one cycle.
const STATUS_PER_CYCLE: usize = 16;
/// The most routes one op slot queues between drains: its SQ share of two
/// lowered records, a quiesce cancel, a close for the handle its completion
/// opened, and a close when its member drains.
const ROUTES_PER_SLOT: usize = 5;
/// The most completions one op slot queues before an io step posts them:
/// its SQ share of two refused records, and its op's own completion.
const READY_PER_SLOT: usize = 3;

/// What backs a resource.
pub(crate) enum Backed {
    Fd(OwnedFd, Class),
    /// The wire at this index of the dispatch's wires.
    Wire(usize),
    /// No system holds it, so it was never opened.
    Unused,
}

pub(crate) struct Resource {
    pub(crate) id: String,
    pub(crate) backed: Backed,
}

/// Something for the io steps to carry out on the backend.
#[derive(Debug)]
pub(crate) enum Route {
    Os(OsOp),
    Cancel(OpId),
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
    /// The end of a sim resource a grant holds.
    end: ResourceEnd,
    open: Open,
}

struct Member {
    handles: Box<[Handle]>,
    grants: usize,
    quiescing: bool,
    drained: bool,
}

impl Member {
    fn new(grants: Vec<(u16, ResourceEnd)>) -> Member {
        let grant = |&(res, end): &(u16, ResourceEnd)| Handle {
            generation: 0,
            res,
            end,
            open: Open::Grant,
        };
        let free = (0..MAX_OPS).map(|_| Handle {
            generation: 0,
            res: 0,
            end: ResourceEnd::Host,
            open: Open::Closed,
        });
        Member {
            handles: grants.iter().map(grant).chain(free).collect(),
            grants: grants.len(),
            quiescing: false,
            drained: false,
        }
    }
}

#[derive(Clone, Copy)]
struct Entry {
    generation: u16,
    res: u16,
    kind: OpKind,
    live: bool,
    canceled: bool,
}

/// Why an op did not run.
enum Refusal {
    /// Answered with this error.
    Reply(IoError),
    /// Not answered: a reply would carry another op's token.
    Drop,
}

impl From<IoError> for Refusal {
    fn from(e: IoError) -> Self {
        Self::Reply(e)
    }
}

/// An SQ record `lower` dropped unanswered.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Rejected;

/// The descriptor or wire an op on a handle goes to.
enum Target {
    Fd(i32, Class),
    /// The wire at this index, from this end.
    Wire(usize, ResourceEnd),
}

pub(crate) struct Dispatch {
    members: Box<[Member]>,
    ops: Box<[Entry]>,
    resources: Box<[Resource]>,
    wires: Box<[Wire]>,
    names: Box<[FrameStr<64>]>,
    counters: Box<[Counters]>,
    reported: Box<[Counters]>,
    next_status: usize,
    routes: Vec<Route>,
    /// Completions no io step has posted yet, oldest first.
    ready: Vec<(usize, Cqe)>,
    /// What a wire finished, before it is raised.
    wired: Vec<OsDone>,
}

impl Dispatch {
    /// One member per system with a `Cap`, given as the resource and end of
    /// each grant, in order. `wires` holds the wires the resources name.
    pub(crate) fn new(
        members: Vec<Vec<(u16, ResourceEnd)>>,
        resources: Vec<Resource>,
        wires: Vec<Wire>,
    ) -> Result<Self, BuildError> {
        let ops = members.len() * MAX_OPS;
        if ops > usize::from(u16::MAX) + 1 {
            let message = format!("{} systems hold a `Cap`, past the limit", members.len());
            return Err(BuildError::IoSystems { message });
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
            members: members.into_iter().map(Member::new).collect(),
            ops: vec![idle; ops].into(),
            names: resources.iter().map(|r| resource_name(&r.id)).collect(),
            resources: resources.into(),
            wires: wires.into(),
            counters: vec![Counters::default(); count].into(),
            reported: vec![Counters::default(); count].into(),
            next_status: 0,
            routes: Vec::with_capacity(ROUTES_PER_SLOT * ops),
            ready: Vec::with_capacity(READY_PER_SLOT * ops),
            wired: Vec::with_capacity(2 * ops),
        })
    }

    /// Lowers one op from member `m`'s queue, whose buffers lie in `arena`.
    /// An op that names no slot of `m`'s, or a slot still in flight, is
    /// dropped unanswered, since its reply would answer another op: `Err`.
    pub(crate) fn lower(&mut self, m: usize, sqe: Sqe, arena: &Arena) -> Result<(), Rejected> {
        if self.members[m].quiescing {
            return Ok(());
        }
        match self.try_lower(m, sqe, arena) {
            Ok(()) => Ok(()),
            Err(Refusal::Drop) => Err(Rejected),
            Err(Refusal::Reply(e)) => {
                self.ready.push((m, Cqe::err(sqe.token, e)));
                Ok(())
            }
        }
    }

    fn try_lower(&mut self, m: usize, sqe: Sqe, arena: &Arena) -> Result<(), Refusal> {
        if sqe.opcode == OpKind::Cancel as u8 {
            self.cancel(m, sqe.token);
            return Ok(());
        }
        let index = index(m, sqe.token.slot()).ok_or(Refusal::Drop)?;
        if self.ops[index].live {
            return Err(Refusal::Drop);
        }
        let op = sqe.op()?;
        let (res, target) = self.target(m, sqe.handle).ok_or(IoError::Stale)?;
        if op == Op::Close {
            self.close(m, sqe.handle);
            self.ready.push((m, Cqe::ok(sqe.token, 0)));
            return Ok(());
        }
        let buf = match op.fills() || op.drains() {
            true => Some(arena.get_slice(sqe.buf).ok_or(IoError::BadBuf)?),
            false => None,
        };
        self.ops[index] = Entry {
            generation: sqe.token.generation(),
            res,
            kind: op.kind(),
            live: true,
            canceled: false,
        };
        self.counters[usize::from(res)].submitted += 1;
        let id = OpId::new(index as u16, sqe.token.generation());
        match target {
            Target::Fd(fd, class) => self.routes.push(Route::Os(OsOp {
                id,
                fd,
                class,
                op,
                buf,
            })),
            Target::Wire(w, end) => {
                let dropped = self.wires[w].run(end, id, op, buf, &mut self.wired);
                self.counters[usize::from(res)].dropped += dropped as u64;
                self.raise_wired();
            }
        }
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
                Backed::Wire(w) => Target::Wire(*w, handle.end),
                Backed::Unused => return None,
            },
        };
        Some((handle.res, target))
    }

    /// Ends a handle. A grant ends only this capability, and its ops in
    /// flight, parked reads on a wire too, run on until they finish or are
    /// canceled. A derived handle's descriptor closes, ending its ops.
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
        match self.resources[usize::from(entry.res)].backed {
            Backed::Wire(w) => {
                self.wires[w].cancel(id, &mut self.wired);
                self.raise_wired();
            }
            _ => self.routes.push(Route::Cancel(id)),
        }
    }

    /// Queues the completion of a finished op for its member's CQ, unless
    /// no system waits for it.
    pub(crate) fn raise(&mut self, done: OsDone) {
        let index = usize::from(done.id.index());
        let Some(entry) = self.ops.get_mut(index) else {
            return;
        };
        if !entry.live || entry.generation != done.id.generation() {
            return;
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
            return;
        }
        count_cqe(
            &mut self.counters[usize::from(entry.res)],
            entry.kind,
            &result,
        );
        let cqe = result.map_or_else(|e| Cqe::err(token, e), |n| Cqe::ok(token, n));
        self.ready.push((m, cqe));
    }

    /// Raises everything a wire finished.
    fn raise_wired(&mut self) {
        let mut wired = core::mem::take(&mut self.wired);
        wired.drain(..).for_each(|done| self.raise(done));
        self.wired = wired;
    }

    /// Moves the ready completions of the members `owns` marks into `out`,
    /// oldest first. The others stay for the io step that posts them.
    pub(crate) fn take_ready(&mut self, owns: &[bool], out: &mut Vec<(usize, Cqe)>) {
        let owned = |&(m, _): &(usize, Cqe)| owns.get(m) == Some(&true);
        out.extend(self.ready.extract_if(.., |r| owned(r)));
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
    pub(crate) fn quiesce_member(&mut self, m: usize) {
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
    pub(crate) fn member_drained(&mut self, m: usize) {
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

    /// Whether member `m` has nothing left in flight since `begin`.
    pub(crate) fn member_idle(&self, m: usize) -> bool {
        let member = &self.members[m];
        let ops = &self.ops[m * MAX_OPS..(m + 1) * MAX_OPS];
        !member.quiescing || (member.drained && ops.iter().all(|e| !e.live))
    }

    pub(crate) fn end_queiscing_member(&mut self, m: usize) {
        self.members[m].quiescing = false;
    }

    /// The routes queued since the last call.
    pub(crate) fn drain_routes(&mut self) -> std::vec::Drain<'_, Route> {
        self.routes.drain(..)
    }

    /// Starts quiescing every member, for teardown.
    pub(crate) fn quiesce_all(&mut self) {
        (0..self.members.len()).for_each(|m| self.quiesce_member(m));
    }

    /// Whether any op is in flight on a descriptor. An op parked on a wire
    /// names no memory the kernel writes.
    pub(crate) fn live(&self) -> bool {
        let real =
            |e: &Entry| !matches!(self.resources[usize::from(e.res)].backed, Backed::Wire(_));
        self.ops.iter().any(|e| e.live && real(e))
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
            let status = self.counters[at].status(now, self.names[at]);
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

/// Counts one finished op: its result, and the bytes it moved.
fn count_cqe(counters: &mut Counters, kind: OpKind, result: &Result<u32, IoError>) {
    counters.completed += 1;
    let byte_count = match result {
        Ok(n) => u64::from(*n),
        Err(_) => {
            counters.failed += 1;
            0
        }
    };
    match kind {
        OpKind::Read | OpKind::Recv => counters.bytes_in += byte_count,
        OpKind::Write | OpKind::Send | OpKind::SendTo => counters.bytes_out += byte_count,
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
