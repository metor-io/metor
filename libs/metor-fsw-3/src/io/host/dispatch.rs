//! `Dispatch`: every system's handles and ops in flight, in tables sized at
//! build.
//!
//! Each system owns a run of the op table as long as its op budget, so a
//! completion finds its system and token by arithmetic. `lower` turns an
//! `Sqe` into a `Route` for the io steps to carry out, and `raise` turns a
//! finished op back into a `Cqe`. A timeout never leaves the dispatch: it
//! lands when `expire` passes its deadline. Nothing here does IO.
//!
//! A grant's capability holds no descriptor. `Connect` and `Listen` on it
//! open one, which a derived handle maps to.

use core::net::SocketAddr;
use core::ptr::NonNull;
use core::time::Duration;
use std::os::fd::{AsRawFd, OwnedFd};

use super::recipe::{PathKind, Recipe, SocketKind};

use metor_component::FrameStr;
use metor_proto::types::Timestamp;
use zerocopy::{FromZeros, IntoBytes};

use super::wire::Wire;
use super::{Class, Make, OpId, OsDone, OsOp, Out};
use crate::coordinator::BuildError;
use crate::io::arena::Arena;
use crate::io::status::{Counters, ResourceStatus};
use crate::io::{
    Cqe, IoError, Op, OpKind, RawAddr, RawHandle, ResourceEnd, ResourceIndex, Span, Sqe, Token,
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
    /// What each connect or listen makes.
    Real(Recipe),
    /// The wire at this index of the dispatch's wires.
    Wire(usize),
    /// No system holds it.
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
    /// Makes the socket of this resource's recipe for this op.
    Socket(OpId, u16),
}

/// A `Cap` a member holds on a resource.
struct Grant {
    res: u16,
    /// The end of a sim resource the grant holds.
    end: ResourceEnd,
    /// False once the system closes the capability, until its next make.
    open: bool,
}

/// A connection an op opened, mapped to a member's handle.
struct Derived {
    res: u16,
    conn: Conn,
}

enum Conn {
    Fd(OwnedFd, Class),
    /// This end of the wire at this index.
    Wire(usize, ResourceEnd),
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

/// A system with a `Cap`: its op budget, and the resource and end of each
/// grant, in order.
pub(crate) struct MemberDef {
    pub(crate) ops: u16,
    pub(crate) grants: Vec<(ResourceIndex, ResourceEnd)>,
}

/// A system's handles: its grants at indexes `0..grants.len()`, then its
/// derived handles.
struct Member {
    grants: Box<[Grant]>,
    derived: Box<[DerivedSlot]>,
    state: MemberState,
}

impl Member {
    /// One derived handle per op the member may have in flight.
    fn new(def: MemberDef) -> Member {
        let grant = |(res, end): (ResourceIndex, ResourceEnd)| Grant {
            res: res.get(),
            end,
            open: true,
        };
        Member {
            grants: def.grants.into_iter().map(grant).collect(),
            derived: (0..def.ops).map(|_| DerivedSlot::default()).collect(),
            state: MemberState::Running,
        }
    }
}

/// The op table is one array shared by every member, and member `m` owns
/// entries `base[m]..base[m + 1]`, one per op of its budget. Maps a member's
/// slot to its entry and back.
struct OpRanges {
    base: Box<[usize]>,
}

impl OpRanges {
    fn new(budgets: impl Iterator<Item = u16>) -> Self {
        let sums = budgets.scan(0, |sum, ops| {
            *sum += usize::from(ops);
            Some(*sum)
        });
        Self {
            base: core::iter::once(0).chain(sums).collect(),
        }
    }

    /// Every member's ops.
    fn total(&self) -> usize {
        self.base.last().copied().unwrap_or(0)
    }

    /// Member `m`'s entries.
    fn range(&self, m: usize) -> core::ops::Range<usize> {
        self.base[m]..self.base[m + 1]
    }

    fn budget(&self, m: usize) -> usize {
        self.range(m).len()
    }

    /// The entry of member `m`'s slot, if the slot is in its budget.
    fn index(&self, m: usize, slot: u16) -> Option<usize> {
        let slot = usize::from(slot);
        (slot < self.budget(m)).then(|| self.base[m] + slot)
    }

    /// The member and slot an entry below `total` belongs to.
    fn owner(&self, index: usize) -> (usize, u16) {
        let m = self.base.partition_point(|&b| b <= index).saturating_sub(1);
        (m, (index - self.base[m]) as u16)
    }
}

/// An op in flight.
#[derive(Clone, Copy)]
struct InFlight {
    generation: u16,
    res: u16,
    /// The index of the handle it was submitted on.
    handle: u16,
    kind: OpKind,
    canceled: bool,
    /// An `Accept`'s or `Connect`'s buffer, which its peer is written to.
    peer: Option<NonNull<[u8]>>,
    /// A timeout's deadline.
    due: Option<Timestamp>,
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

/// What an op on a handle goes to.
#[derive(Clone, Copy)]
enum Target {
    /// A capability on a real resource.
    Recipe,
    /// A capability on the wire at this index, from this end.
    WireCap(usize, ResourceEnd),
    Fd(i32, Class),
    /// A connection to the wire at this index, from this end.
    Wire(usize, ResourceEnd),
}

impl Target {
    fn is_cap(self) -> bool {
        matches!(self, Self::Recipe | Self::WireCap(..))
    }
}

pub(crate) struct Dispatch {
    members: Box<[Member]>,
    ranges: OpRanges,
    ops: Box<[Option<InFlight>]>,
    /// How many ops in `ops` are timeouts.
    timers: usize,
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
    /// One member per system with a `Cap`. `wires` holds the wires the
    /// resources name.
    pub(crate) fn new(
        members: Vec<MemberDef>,
        resources: Vec<Resource>,
        wires: Vec<Wire>,
    ) -> Result<Self, BuildError> {
        let ranges = OpRanges::new(members.iter().map(|m| m.ops));
        let ops = Self::check_ops(ranges.total())?;
        let count = resources.len();
        Ok(Self {
            members: members.into_iter().map(Member::new).collect(),
            ranges,
            ops: vec![None; ops].into(),
            timers: 0,
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

    /// `ops`, if an `OpId` can name each of that many ops.
    pub(crate) fn check_ops(ops: usize) -> Result<usize, BuildError> {
        match ops <= usize::from(u16::MAX) + 1 {
            true => Ok(ops),
            false => Err(BuildError::TooManyIoOps { ops }),
        }
    }

    /// Lowers one op from member `m`'s queue at cycle time `now`, whose
    /// buffers lie in `arena`. An op that names no slot of `m`'s, or a slot
    /// still in flight, is dropped unanswered, since its reply would answer
    /// another op: `Err`.
    pub(crate) fn lower(
        &mut self,
        m: usize,
        sqe: Sqe,
        arena: &Arena,
        now: Timestamp,
    ) -> Result<(), Rejected> {
        if self.members[m].state != MemberState::Running {
            return Ok(());
        }
        match self.try_lower(m, sqe, arena, now) {
            Ok(()) => Ok(()),
            Err(Refusal::Drop) => Err(Rejected),
            Err(Refusal::Reply(e)) => {
                self.ready.push((m, Cqe::err(sqe.token, e)));
                Ok(())
            }
        }
    }

    fn try_lower(
        &mut self,
        m: usize,
        sqe: Sqe,
        arena: &Arena,
        now: Timestamp,
    ) -> Result<(), Refusal> {
        let op = sqe.op();
        if op == Ok(Op::Cancel) {
            self.cancel(m, sqe.token);
            return Ok(());
        }
        let index = self
            .ranges
            .index(m, sqe.token.index())
            .ok_or(Refusal::Drop)?;
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
        if let Op::Timeout(after) = op {
            self.start_timer(index, res, sqe.token.generation(), deadline(now, after));
            return Ok(());
        }
        if target.is_cap() != matches!(op, Op::Connect | Op::Listen) {
            return Err(IoError::Unsupported.into());
        }
        if let Target::WireCap(w, end) = target {
            self.connect_wire(m, res, w, end, op, sqe.token);
            return Ok(());
        }
        self.start(index, res, target, op, sqe, arena)
    }

    /// Puts op `index` in flight and routes it to its target.
    fn start(
        &mut self,
        index: usize,
        res: u16,
        target: Target,
        op: Op,
        sqe: Sqe,
        arena: &Arena,
    ) -> Result<(), Refusal> {
        let buf = lent(&op, sqe.buf, arena)?;
        let id = OpId::new(index as u16, sqe.token.generation());
        let route = match target {
            Target::Fd(fd, class) => Some(Route::Os(OsOp {
                id,
                fd,
                class,
                op,
                buf,
                make: None,
            })),
            Target::Recipe => Some(self.make(res, id, op, buf)?),
            Target::Wire(..) | Target::WireCap(..) => None,
        };
        self.ops[index] = Some(InFlight {
            generation: sqe.token.generation(),
            res,
            handle: sqe.handle.index(),
            kind: op.kind(),
            canceled: false,
            peer: buf.filter(|_| matches!(op, Op::Accept | Op::Connect)),
            due: None,
        });
        self.counters[usize::from(res)].submitted += 1;
        match target {
            Target::Wire(w, end) => {
                let dropped = self.wires[w].run(end, id, op, buf, &mut self.wired);
                self.counters[usize::from(res)].dropped += dropped as u64;
                self.raise_wired();
            }
            _ => self.routes.extend(route),
        }
        Ok(())
    }

    /// The route that makes a connection from resource `res`'s recipe.
    fn make(
        &self,
        res: u16,
        id: OpId,
        op: Op,
        buf: Option<NonNull<[u8]>>,
    ) -> Result<Route, IoError> {
        let Backed::Real(recipe) = &self.resources[usize::from(res)].backed else {
            return Err(IoError::Unsupported);
        };
        let os = |class, make| {
            Route::Os(OsOp {
                id,
                fd: -1,
                class,
                op,
                buf,
                make: Some(make),
            })
        };
        Ok(match (recipe, op) {
            (Recipe::Path(kind), Op::Connect) => os(
                kind.class(),
                Make::Open {
                    path: NonNull::from(kind.path()).cast(),
                    flags: kind.flags(),
                },
            ),
            (Recipe::Socket(SocketKind::Dial(to)), Op::Connect) => {
                os(Class::Stream, Make::Dial(*to))
            }
            (Recipe::Socket(SocketKind::Listen { .. }), Op::Listen)
            | (Recipe::Socket(SocketKind::Udp { .. } | SocketKind::Can { .. }), Op::Connect) => {
                Route::Socket(id, res)
            }
            _ => return Err(IoError::Unsupported),
        })
    }

    /// Connects member `m` to end `end` of wire `w`, landing at once.
    fn connect_wire(
        &mut self,
        m: usize,
        res: u16,
        w: usize,
        end: ResourceEnd,
        op: Op,
        token: Token,
    ) {
        let result = match op {
            Op::Connect => self.derive(m, res, Conn::Wire(w, end)),
            _ => Err(IoError::Unsupported),
        };
        let counters = &mut self.counters[usize::from(res)];
        counters.submitted += 1;
        counters.record_completion(op.kind(), &result);
        let cqe = result.map_or_else(|e| Cqe::err(token, e), |n| Cqe::ok(token, n));
        self.ready.push((m, cqe));
    }

    /// The socket recipe of resource `res`, for a `Route::Socket`.
    pub(crate) fn socket(&self, res: u16) -> Option<&SocketKind> {
        match &self.resources.get(usize::from(res))?.backed {
            Backed::Real(Recipe::Socket(kind)) => Some(kind),
            _ => None,
        }
    }

    /// The path recipe op `id` connects to, if it is a path's open in flight.
    pub(crate) fn opening(&self, id: OpId) -> Option<&PathKind> {
        let op = self.ops.get(usize::from(id.index()))?.as_ref()?;
        if op.generation != id.generation() || op.kind != OpKind::Connect {
            return None;
        }
        match &self.resources[usize::from(op.res)].backed {
            Backed::Real(Recipe::Path(kind)) => Some(kind),
            _ => None,
        }
    }

    /// Puts a timeout in flight at op `index`, landing at `at`.
    fn start_timer(&mut self, index: usize, res: u16, generation: u16, at: Timestamp) {
        self.ops[index] = Some(InFlight {
            generation,
            res,
            handle: 0,
            kind: OpKind::Timeout,
            canceled: false,
            peer: None,
            due: Some(at),
        });
        self.timers += 1;
        self.counters[usize::from(res)].submitted += 1;
    }

    /// Lands every timeout whose deadline is at or before `now`.
    pub(crate) fn expire(&mut self, now: Timestamp) {
        for index in 0..self.ops.len() {
            if self.timers == 0 {
                return;
            }
            let Some(op) = &mut self.ops[index] else {
                continue;
            };
            if op.due.take_if(|at| *at <= now).is_none() {
                continue;
            }
            self.timers -= 1;
            let id = OpId::new(index as u16, op.generation);
            self.raise(OsDone {
                id,
                result: Ok(Out::Len(0)),
            });
        }
    }

    /// The live handle `raw` names in member `m`, with its resource.
    fn target(&self, m: usize, raw: RawHandle) -> Option<(u16, Target)> {
        let member = &self.members[m];
        let index = usize::from(raw.index());
        let Some(slot) = index.checked_sub(member.grants.len()) else {
            let grant = member.grants.get(index).filter(|g| g.open)?;
            let target = match &self.resources[usize::from(grant.res)].backed {
                Backed::Real(_) => Target::Recipe,
                Backed::Wire(w) => Target::WireCap(*w, grant.end),
                Backed::Unused => return None,
            };
            return (raw.generation() == 0).then_some((grant.res, target));
        };
        let slot = member.derived.get(slot)?;
        let derived = slot.derived.as_ref()?;
        let target = match &derived.conn {
            Conn::Fd(fd, class) => Target::Fd(fd.as_raw_fd(), *class),
            Conn::Wire(w, end) => Target::Wire(*w, *end),
        };
        (slot.generation == raw.generation()).then_some((derived.res, target))
    }

    /// Ends a handle. A grant ends only this capability, and its ops in
    /// flight run on until they finish or are canceled. A derived handle's
    /// connection closes, ending its ops.
    fn close(&mut self, m: usize, raw: RawHandle) {
        let member = &mut self.members[m];
        let index = usize::from(raw.index());
        let derived = match index.checked_sub(member.grants.len()) {
            None => {
                member.grants[index].open = false;
                return;
            }
            Some(slot) => member.derived[slot].derived.take(),
        };
        match derived.map(|d| d.conn) {
            Some(Conn::Fd(fd, _)) => self.routes.push(Route::Close(fd)),
            Some(Conn::Wire(..)) => self.cancel_on(m, raw.index()),
            None => {}
        }
    }

    /// Cancels member `m`'s ops on handle index `handle`.
    fn cancel_on(&mut self, m: usize, handle: u16) {
        for index in self.ranges.range(m) {
            if let Some(op) = self.ops[index].filter(|op| op.handle == handle) {
                let (_, slot) = self.ranges.owner(index);
                self.cancel(m, Token::new(slot, op.generation));
            }
        }
    }

    /// Cancels member `m`'s op `token`, once.
    fn cancel(&mut self, m: usize, token: Token) {
        let Some(index) = self.ranges.index(m, token.index()) else {
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
        if op.due.take().is_some() {
            self.timers -= 1;
            let result = Err(IoError::Canceled);
            return self.raise(OsDone { id, result });
        }
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
        let (m, slot) = self.ranges.owner(index);
        let token = Token::new(slot, op.generation);
        let result = match done.result {
            Ok(Out::Len(n)) => Ok(n),
            Ok(Out::Opened { fd, peer, .. })
                if op.kind == OpKind::Accept && !self.allows(op.res, peer) =>
            {
                self.turn_away(op.res, fd)
            }
            Ok(Out::Opened { fd, class, peer }) => {
                write_peer(&op, peer);
                self.derive(m, op.res, Conn::Fd(fd, class))
            }
            Err(e) => Err(e),
        };
        if self.members[m].state != MemberState::Running {
            return;
        }
        self.counters[usize::from(op.res)].record_completion(op.kind, &result);
        let cqe = result.map_or_else(|e| Cqe::err(token, e), |n| Cqe::ok(token, n));
        self.ready.push((m, cqe));
    }

    /// Whether resource `res`, if a listener, admits `peer`.
    fn allows(&self, res: u16, peer: Option<SocketAddr>) -> bool {
        let Backed::Real(Recipe::Socket(SocketKind::Listen { allow, .. })) =
            &self.resources[usize::from(res)].backed
        else {
            return true;
        };
        allow.is_empty() || peer.is_some_and(|p| allow.iter().any(|c| c.contains(p.ip())))
    }

    /// Closes a stream whose peer is not allowed, and refuses its op.
    fn turn_away(&mut self, res: u16, fd: OwnedFd) -> Result<u32, IoError> {
        self.routes.push(Route::Close(fd));
        self.counters[usize::from(res)].rejected += 1;
        Err(IoError::Refused)
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

    /// How many completions an io step posting to `members` may hold.
    pub(crate) fn held_for(&self, members: &[usize]) -> usize {
        members.iter().map(|&m| self.ops_of(m)).sum::<usize>() * HELD_PER_SLOT
    }

    /// Member `m`'s op budget.
    pub(crate) fn ops_of(&self, m: usize) -> usize {
        self.ranges.budget(m)
    }

    /// Every member's ops.
    pub(crate) fn ops(&self) -> usize {
        self.ops.len()
    }

    /// Maps a new connection to a free derived handle of member `m`, or
    /// closes it when none is free or the member is quiescing.
    fn derive(&mut self, m: usize, res: u16, conn: Conn) -> Result<u32, IoError> {
        let member = &mut self.members[m];
        let free = member.derived.iter().position(|d| d.derived.is_none());
        let (Some(free), MemberState::Running) = (free, member.state) else {
            if let Conn::Fd(fd, _) = conn {
                self.routes.push(Route::Close(fd));
            }
            return Err(IoError::Refused);
        };
        let slot = &mut member.derived[free];
        slot.generation = slot.generation.wrapping_add(1);
        slot.derived = Some(Derived { res, conn });
        let index = member.grants.len() + free;
        Ok(RawHandle::new(index as u16, slot.generation).get())
    }

    /// Starts quiescing member `m`: drops its new ops and cancels those in
    /// flight.
    pub(crate) fn quiesce_member(&mut self, m: usize) {
        self.members[m].state = MemberState::Quiescing;
        for index in self.ranges.range(m) {
            if let Some(op) = self.ops[index] {
                let (_, slot) = self.ranges.owner(index);
                self.cancel(m, Token::new(slot, op.generation));
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
            .extend(closed.filter_map(|derived| match derived.conn {
                Conn::Fd(fd, _) => Some(Route::Close(fd)),
                Conn::Wire(..) => None,
            }));
    }

    /// Whether member `m` has nothing left in flight since `begin`.
    pub(crate) fn member_idle(&self, m: usize) -> bool {
        let ops = &self.ops[self.ranges.range(m)];
        match self.members[m].state {
            MemberState::Running => true,
            MemberState::Quiescing => false,
            MemberState::Drained => ops.iter().all(Option::is_none),
        }
    }

    pub(crate) fn end_quiescing_member(&mut self, m: usize) {
        self.members[m].state = MemberState::Running;
    }

    /// The routes queued since the last call.
    pub(crate) fn drain_routes(&mut self) -> std::vec::Drain<'_, Route> {
        self.routes.drain(..)
    }

    /// Takes the queued routes, to carry out while the dispatch is read.
    /// [`put_routes`](Self::put_routes) hands the emptied list back.
    pub(crate) fn take_routes(&mut self) -> Vec<Route> {
        core::mem::take(&mut self.routes)
    }

    pub(crate) fn put_routes(&mut self, mut routes: Vec<Route>) {
        routes.clear();
        routes.append(&mut self.routes);
        self.routes = routes;
    }

    /// Starts quiescing every member, for teardown.
    pub(crate) fn quiesce_all(&mut self) {
        (0..self.members.len()).for_each(|m| self.quiesce_member(m));
    }

    /// Whether any op is in flight on a descriptor. An op parked on a wire
    /// names no memory the kernel writes.
    pub(crate) fn has_kernel_ops_in_flight(&self) -> bool {
        let real = |op: &InFlight| {
            let wired = matches!(self.resources[usize::from(op.res)].backed, Backed::Wire(_));
            !wired && op.kind != OpKind::Timeout
        };
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
}

/// The arena memory an op lends: its buffer if it needs one, or a
/// connect's peer buffer if it names one.
fn lent(op: &Op, span: Span, arena: &Arena) -> Result<Option<NonNull<[u8]>>, IoError> {
    if !op.needs_buf() && !(op.fills() && !span.is_empty()) {
        return Ok(None);
    }
    let buf = arena.get_slice(span).ok_or(IoError::BadBuf)?;
    match buf.len() < op.min_buf() {
        true => Err(IoError::BadBuf),
        false => Ok(Some(buf)),
    }
}

/// `after` past `now`, saturating.
fn deadline(now: Timestamp, after: Duration) -> Timestamp {
    let micros = i64::try_from(after.as_micros()).unwrap_or(i64::MAX);
    Timestamp(now.0.saturating_add(micros))
}

/// Writes `peer`'s `RawAddr` into an `Accept`'s or `Connect`'s buffer, or
/// zeroes for a connection with no peer.
fn write_peer(op: &InFlight, peer: Option<SocketAddr>) {
    let Some(buf) = op.peer else {
        return;
    };
    let addr = peer.map_or_else(RawAddr::new_zeroed, RawAddr::from);
    // SAFETY: the op lent its buffer until its completion, which is being
    // raised now, and the io steps run on one thread.
    let bytes = unsafe { &mut *buf.as_ptr() };
    let _ = addr.write_to_prefix(bytes);
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Two members of any budgets: an entry gives back its member and slot,
    /// so no two slots share one, and it fits an `OpId`.
    #[kani::proof]
    fn index_never_aliases() {
        let budgets: [u16; 2] = kani::any();
        let slots = OpRanges::new(budgets.into_iter());
        kani::assume(slots.total() <= usize::from(u16::MAX) + 1);
        let (m, slot): (bool, u16) = (kani::any(), kani::any());
        let m = usize::from(m);
        let Some(index) = slots.index(m, slot) else {
            assert!(slot >= budgets[m]);
            return;
        };
        assert!(slots.range(m).contains(&index));
        assert_eq!(slots.owner(index), (m, slot));
        let id = OpId::new(index as u16, kani::any());
        assert_eq!(usize::from(id.index()), index);
    }
}
