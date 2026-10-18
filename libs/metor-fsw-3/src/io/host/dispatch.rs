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
use crate::io::status::{Counters, ResourceStatus};
use crate::io::{
    Cqe, IoError, MAX_OPS, Op, OpKind, RawHandle, ResourceEnd, ResourceIndex, Sqe, Token,
};
use crate::port::Output;

/// The most `io_status` records written in one cycle.
const STATUS_PER_CYCLE: usize = 16;
/// The SQ records one op slot may have lowered in one pass.
const LOWERED_PER_SLOT: usize = 2;
const QUIESCE_CANCEL: usize = 1;
const CLOSE_OPENED: usize = 1;
const CLOSE_ON_DRAIN: usize = 1;
const OWN_COMPLETION: usize = 1;
/// The most routes one op slot queues between drains.
const ROUTES_PER_SLOT: usize = LOWERED_PER_SLOT + QUIESCE_CANCEL + CLOSE_OPENED + CLOSE_ON_DRAIN;
/// The completions of one op slot a full CQ may hold back.
const HELD_PER_SLOT: usize = 2;
/// The most completions one op slot queues before an io step posts them.
const READY_PER_SLOT: usize = LOWERED_PER_SLOT + OWN_COMPLETION + HELD_PER_SLOT;

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

/// A `Cap` a member holds on a resource.
struct Grant {
    res: u16,
    /// The end of a sim resource the grant holds.
    end: ResourceEnd,
    /// False once the system closes the capability, until its next make.
    open: bool,
}

/// A descriptor an op opened, mapped to a member's handle.
struct Derived {
    res: u16,
    fd: OwnedFd,
    class: Class,
}

/// A handle slot for a descriptor an op opens.
#[derive(Default)]
struct DerivedSlot {
    generation: u16,
    derived: Option<Derived>,
}

/// Where a member is in a quiesce.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MemberState {
    Running,
    /// Dropping new ops and waiting for its SQ to drain.
    Quiescing,
    /// Its SQ drained, its derived handles closed, and its grants reopened.
    Drained,
}

/// A system's handles: its grants at indexes `0..grants.len()`, then its
/// derived handles.
struct Member {
    grants: Box<[Grant]>,
    derived: Box<[DerivedSlot]>,
    state: MemberState,
}

impl Member {
    fn new(grants: Vec<(ResourceIndex, ResourceEnd)>) -> Member {
        let grant = |(res, end): (ResourceIndex, ResourceEnd)| Grant {
            res: res.get(),
            end,
            open: true,
        };
        Member {
            grants: grants.into_iter().map(grant).collect(),
            derived: (0..MAX_OPS).map(|_| DerivedSlot::default()).collect(),
            state: MemberState::Running,
        }
    }
}

/// An op in flight.
#[derive(Clone, Copy)]
struct InFlight {
    generation: u16,
    res: u16,
    kind: OpKind,
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
    ops: Box<[Option<InFlight>]>,
    resources: Box<[Resource]>,
    wires: Box<[Wire]>,
    names: Box<[FrameStr<64>]>,
    counters: Box<[Counters]>,
    reported: Box<[Counters]>,
    next_status: usize,
    routes: Vec<Route>,
    /// Completions no io step has posted yet, oldest first, including those
    /// a full CQ held back.
    ready: Vec<(usize, Cqe)>,
    /// What a wire finished, before it is raised.
    wired: Vec<OsDone>,
}

impl Dispatch {
    /// One member per system with a `Cap`, given as the resource and end of
    /// each grant, in order. `wires` holds the wires the resources name.
    pub(crate) fn new(
        members: Vec<Vec<(ResourceIndex, ResourceEnd)>>,
        resources: Vec<Resource>,
        wires: Vec<Wire>,
    ) -> Result<Self, BuildError> {
        let ops = members.len() * MAX_OPS;
        if ops > usize::from(u16::MAX) + 1 {
            let count = members.len();
            return Err(BuildError::TooManyIoMembers { count });
        }
        let count = resources.len();
        Ok(Self {
            members: members.into_iter().map(Member::new).collect(),
            ops: vec![None; ops].into(),
            names: resources
                .iter()
                .map(|r| FrameStr::truncated(&r.id))
                .collect(),
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
        if self.members[m].state != MemberState::Running {
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
        let op = sqe.op();
        if op == Ok(Op::Cancel) {
            self.cancel(m, sqe.token);
            return Ok(());
        }
        let index = Self::op_index(m, sqe.token.index()).ok_or(Refusal::Drop)?;
        if self.ops[index].is_some() {
            return Err(Refusal::Drop);
        }
        let op = op?;
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
        self.ops[index] = Some(InFlight {
            generation: sqe.token.generation(),
            res,
            kind: op.kind(),
            canceled: false,
        });
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
        let member = &self.members[m];
        let index = usize::from(raw.index());
        let Some(slot) = index.checked_sub(member.grants.len()) else {
            let grant = member.grants.get(index).filter(|g| g.open)?;
            let target = match &self.resources[usize::from(grant.res)].backed {
                Backed::Fd(fd, class) => Target::Fd(fd.as_raw_fd(), *class),
                Backed::Wire(w) => Target::Wire(*w, grant.end),
                Backed::Unused => return None,
            };
            return (raw.generation() == 0).then_some((grant.res, target));
        };
        let slot = member.derived.get(slot)?;
        let derived = slot.derived.as_ref()?;
        let target = Target::Fd(derived.fd.as_raw_fd(), derived.class);
        (slot.generation == raw.generation()).then_some((derived.res, target))
    }

    /// Ends a handle. A grant ends only this capability, and its ops in
    /// flight, parked reads on a wire too, run on until they finish or are
    /// canceled. A derived handle's descriptor closes, ending its ops.
    fn close(&mut self, m: usize, raw: RawHandle) {
        let member = &mut self.members[m];
        let index = usize::from(raw.index());
        match index.checked_sub(member.grants.len()) {
            None => member.grants[index].open = false,
            Some(slot) => {
                if let Some(derived) = member.derived[slot].derived.take() {
                    self.routes.push(Route::Close(derived.fd));
                }
            }
        }
    }

    /// Cancels member `m`'s op `token`, once.
    fn cancel(&mut self, m: usize, token: Token) {
        let Some(index) = Self::op_index(m, token.index()) else {
            return;
        };
        let Some(op) = &mut self.ops[index] else {
            return;
        };
        if op.canceled || op.generation != token.generation() {
            return;
        }
        op.canceled = true;
        let id = OpId::new(index as u16, op.generation);
        match self.resources[usize::from(op.res)].backed {
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
        debug_assert!(index < self.ops.len(), "an id from this dispatch's table");
        // PANIC Safety: every `OpId` comes from this dispatch's op table.
        let ours = |op: &mut InFlight| op.generation == done.id.generation();
        let Some(op) = self.ops[index].take_if(ours) else {
            return;
        };
        let m = index / MAX_OPS;
        let token = Token::new((index % MAX_OPS) as u16, op.generation);
        let result = match done.result {
            Ok(Out::Len(n)) => Ok(n),
            Ok(Out::Opened(fd, class)) => self.derive(m, op.res, fd, class),
            Err(e) => Err(e),
        };
        if self.members[m].state != MemberState::Running {
            return;
        }
        self.counters[usize::from(op.res)].record_completion(op.kind, &result);
        let cqe = result.map_or_else(|e| Cqe::err(token, e), |n| Cqe::ok(token, n));
        self.ready.push((m, cqe));
    }

    /// Raises everything a wire finished.
    fn raise_wired(&mut self) {
        let mut wired = core::mem::take(&mut self.wired);
        wired.drain(..).for_each(|done| self.raise(done));
        self.wired = wired;
    }

    /// Posts the ready completions through `post`, oldest first.
    ///
    /// `post` returns `None` for a member another io step owns, which keeps
    /// the completion. Up to `held` completions `post` refuses are held for
    /// the next pass; past that they are lost, and this returns `true`.
    pub(crate) fn post_ready(
        &mut self,
        held: usize,
        mut post: impl FnMut(usize, &Cqe) -> Option<bool>,
    ) -> bool {
        let (mut kept, mut lost) = (0, false);
        self.ready.retain(|(m, cqe)| match post(*m, cqe) {
            None => true,
            Some(true) => false,
            Some(false) => {
                kept += 1;
                lost |= kept > held;
                kept <= held
            }
        });
        lost
    }

    /// How many completions an io step owning `members` members may hold.
    pub(crate) fn held_for(members: usize) -> usize {
        members * MAX_OPS * HELD_PER_SLOT
    }

    /// Maps a new descriptor to a free derived handle of member `m`, or closes
    /// it when none is free or the member is quiescing.
    fn derive(&mut self, m: usize, res: u16, fd: OwnedFd, class: Class) -> Result<u32, IoError> {
        let member = &mut self.members[m];
        let free = member.derived.iter().position(|d| d.derived.is_none());
        let (Some(free), MemberState::Running) = (free, member.state) else {
            self.routes.push(Route::Close(fd));
            return Err(IoError::Refused);
        };
        let slot = &mut member.derived[free];
        slot.generation = slot.generation.wrapping_add(1);
        slot.derived = Some(Derived { res, fd, class });
        let index = member.grants.len() + free;
        Ok(RawHandle::new(index as u16, slot.generation).get())
    }

    /// Starts quiescing member `m`: drops its new ops and cancels those in
    /// flight.
    pub(crate) fn quiesce_member(&mut self, m: usize) {
        self.members[m].state = MemberState::Quiescing;
        for slot in 0..MAX_OPS {
            if let Some(op) = self.ops[m * MAX_OPS + slot] {
                self.cancel(m, Token::new(slot as u16, op.generation));
            }
        }
    }

    /// Notes that a quiescing member's queue is drained. The first time,
    /// closes its derived handles and revives its grants for the next make.
    pub(crate) fn member_drained(&mut self, m: usize) {
        let member = &mut self.members[m];
        if member.state != MemberState::Quiescing {
            return;
        }
        member.state = MemberState::Drained;
        member.grants.iter_mut().for_each(|g| g.open = true);
        let closed = member.derived.iter_mut().filter_map(|d| d.derived.take());
        self.routes
            .extend(closed.map(|derived| Route::Close(derived.fd)));
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
    pub(crate) fn has_kernel_ops_in_flight(&self) -> bool {
        let real =
            |op: &InFlight| !matches!(self.resources[usize::from(op.res)].backed, Backed::Wire(_));
        self.ops.iter().flatten().any(real)
    }

    /// Writes up to `STATUS_PER_CYCLE` changed resources, starting after the
    /// last one written. A record the ring refuses is written again later.
    pub(crate) fn report(&mut self, now: Timestamp, out: &mut Output<ResourceStatus>) {
        let count = self.counters.len();
        let mut written = 0;
        for step in 0..count {
            let index = (self.next_status + step) % count;
            if self.counters[index] == self.reported[index] {
                continue;
            }
            let status = self.counters[index].status(now, self.names[index]);
            if written == STATUS_PER_CYCLE || out.write(&status).is_err() {
                self.next_status = index;
                return;
            }
            self.reported[index] = self.counters[index];
            written += 1;
        }
    }

    /// The op table index of member `m`'s slot, if the slot is one.
    fn op_index(m: usize, slot: u16) -> Option<usize> {
        let slot = usize::from(slot);
        (slot < MAX_OPS).then_some(m * MAX_OPS + slot)
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
        if let Some(index) = Dispatch::op_index(m, slot) {
            assert_eq!((index / MAX_OPS, index % MAX_OPS), (m, usize::from(slot)));
            let id = OpId::new(index as u16, kani::any());
            assert_eq!(usize::from(id.index()), index);
        }
    }
}
