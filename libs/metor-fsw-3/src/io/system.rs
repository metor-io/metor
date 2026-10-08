//! `io_submit` and `io_complete`: the two steps that carry every IO system's
//! ops to the reactor and their completions back.
//!
//! `io_submit` runs after every IO system. It reads each member's SQ, maps
//! the system's handles to the `Os`'s, and pushes the ops, then hands them to
//! the kernel. `io_complete` runs later in the cycle: it reaps what has
//! landed and writes each completion to its member's CQ, which the system
//! reads through a loop edge on the next cycle. It also writes `io_status`.

use core::sync::atomic::AtomicU32;
use std::borrow::Cow;
use std::rc::Rc;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use crate::coordinator::{
    MakeCx, ParamError, Step, SystemKind, TableEntry, bindings, refuse_thread,
};
use crate::def::{DefCx, DefError};
use crate::fn_system::{LOG_PORT, report_dropped};
use crate::log::LogPort;
use crate::port::{Input, Output, dyn_input_defs, dyn_output_defs};
use crate::system::{OutputBinding, SystemDef};

use super::Token;
use super::handles::Handles;
use super::shared::{Member, SharedIo};
use super::status::{Counters, ResourceStatus, resource_name};
use super::{
    Backing, Cqe, IoError, Limits, MAX_OPS, Op, OpKind, Opened, Os, OsError, Out, RawAddr,
    RawHandle, Reactor, SourceId, Sqe, Tables, open,
};

pub const IO_SUBMIT_TYPE: &str = "fsw.io_submit";
pub const IO_COMPLETE_TYPE: &str = "fsw.io_complete";
/// The output `io_complete` writes resource counts to.
pub const IO_STATUS_PORT: &str = "io_status";
/// The most `io_status` records written in one cycle.
const STATUS_PER_CYCLE: usize = 16;

/// Builds an `Os` over the target's tables and dial targets.
pub(crate) type MakeOs = fn(&Limits, Tables, &[RawAddr]) -> Result<Box<dyn Os>, OsError>;

/// What one op in flight is, so its completion can be counted.
#[derive(Clone, Copy)]
struct InFlight {
    res: u16,
    kind: OpKind,
}

/// One member as the io steps see it.
pub(crate) struct HostMember {
    pub(crate) member: Rc<Member>,
    src: SourceId,
    handles: Handles,
    /// Indexed by the op token's slot.
    ops: Box<[Option<InFlight>]>,
}

/// The io steps' state: the reactor every member's ops go through.
pub(crate) struct IoHost {
    pub(crate) reactor: Reactor,
    members: Box<[HostMember]>,
    counters: Box<[Counters]>,
    reported: Box<[Counters]>,
    names: Box<[metor_component::FrameStr<64>]>,
    /// Ops `io_submit` answered itself, by member index.
    early: Vec<(usize, Cqe)>,
    next_status: usize,
}

pub(crate) fn submit_entry() -> TableEntry {
    submit_entry_with(super::host)
}

/// `io_submit`'s entry, over the `Os` that `make_os` builds.
pub(crate) fn submit_entry_with(make_os: MakeOs) -> TableEntry {
    TableEntry {
        kind: SystemKind::Cyclic,
        // PANIC Safety: the empty context names no ports, which cannot fail.
        def: submit_def(&DefCx::empty()).expect("a static definition"),
        def_fn: Box::new(submit_def),
        doc: "Carries every IO system's ops to the reactor.".into(),
        schema: None,
        make: Arc::new(move |cx| make_submit(cx, make_os)),
    }
}

pub(crate) fn complete_entry() -> TableEntry {
    TableEntry {
        kind: SystemKind::Cyclic,
        // PANIC Safety: as in `submit_entry`.
        def: complete_def(&DefCx::empty()).expect("a static definition"),
        def_fn: Box::new(complete_def),
        doc: "Carries every IO system's completions back, and writes io_status.".into(),
        schema: None,
        make: Arc::new(make_complete),
    }
}

/// One input per member, named after it, then `log`.
fn submit_def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    Ok(SystemDef {
        name: "io_submit".into(),
        inputs: dyn_input_defs(cx, &[])?,
        outputs: vec![Output::<LogEvent>::def(cx, LOG_PORT)],
        grants: Vec::new(),
    })
}

/// `log`, `io_status`, then one CQ per member, named after it.
fn complete_def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let fixed = [Cow::Borrowed(LOG_PORT), Cow::Borrowed(IO_STATUS_PORT)];
    let mut outputs = vec![
        Output::<LogEvent>::def(cx, LOG_PORT),
        Output::<ResourceStatus>::def(cx, IO_STATUS_PORT),
    ];
    outputs.extend(dyn_output_defs(cx, &fixed)?);
    Ok(SystemDef {
        name: "io_complete".into(),
        inputs: Vec::new(),
        outputs,
        grants: Vec::new(),
    })
}

/// `io_submit`'s make, over the `Os` that `make_os` builds.
fn make_submit(cx: MakeCx<'_>, make_os: MakeOs) -> Result<Box<dyn Step>, ParamError> {
    refuse_thread(cx.adapter)?;
    let (views, writers) = bindings(cx.def, cx.inputs, cx.outputs);
    let names: Vec<&str> = cx.def.inputs.iter().map(|p| p.name.as_ref()).collect();
    let host = IoHost::new(&cx.io.borrow(), &names, make_os)?;
    cx.io.borrow_mut().host = Some(host);
    let sqs = views.into_iter().map(|b| Input::new(b.views)).collect();
    Ok(Box::new(SubmitStep {
        io: cx.io.clone(),
        sqs,
        log: log_port(writers),
    }))
}

fn make_complete(cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError> {
    refuse_thread(cx.adapter)?;
    if cx.io.borrow().host.is_none() {
        return Err(ParamError::NoIoSubmit);
    }
    let (_, writers) = bindings(cx.def, cx.inputs, cx.outputs);
    let mut writers = writers.into_iter();
    let log = log_port(writers.by_ref().take(1).collect());
    // PANIC Safety: `complete_def` declares `io_status` second.
    let status = Output::new(writers.next().expect("the io_status output").writer);
    let dropped = Arc::<AtomicU32>::default();
    let cq = |b: OutputBinding| Output::counting(b.writer, dropped.clone());
    let cqs = writers.map(cq).collect::<Vec<_>>();
    // The reactor returns at most every token it holds, three per tracked op.
    let done = Vec::with_capacity(6 * cqs.len().max(1) * MAX_OPS);
    Ok(Box::new(CompleteStep {
        io: cx.io.clone(),
        cqs,
        status,
        log,
        dropped,
        done,
    }))
}

/// The `log` output, the only writer in `writers`.
fn log_port(writers: Vec<OutputBinding>) -> LogPort {
    // PANIC Safety: both io steps declare `log` first among their outputs.
    let log = writers.into_iter().next().expect("the log output");
    LogPort::new(Output::new(log.writer))
}

impl IoHost {
    /// Opens the members' resources, freezes their tables, and builds the
    /// reactor. `names` are `io_submit`'s inputs, one per member, in order.
    fn new(shared: &super::IoShared, names: &[&str], make_os: MakeOs) -> Result<Self, ParamError> {
        let members = names
            .iter()
            .map(|name| shared.find(name).ok_or_else(|| no_member(name)))
            .collect::<Result<Vec<_>, _>>()?;
        let wanted = wanted(shared, &members)?;
        let opened = open_all(shared, &wanted)?;
        members.iter().for_each(|m| m.table.bufs.freeze());
        let limits = Limits {
            // Each member's tracked ops, plus the cancels and closes it sends
            // untracked.
            ops: 2 * MAX_OPS * members.len().max(1),
            sources: members.len(),
        };
        let dials: Vec<RawAddr> = opened.iter().filter_map(dial).collect();
        let mut os = make_os(&limits, shared.tables.clone(), &dials).map_err(os_error)?;
        let handles = register(&mut *os, opened, dials.len());
        let mut reactor = Reactor::new(os, limits);
        let members = members
            .into_iter()
            .map(|member| host_member(shared, &mut reactor, member, &handles))
            .collect::<Result<_, _>>()?;
        let count = shared.resources.len();
        Ok(Self {
            reactor,
            members,
            counters: vec![Counters::default(); count].into(),
            reported: vec![Counters::default(); count].into(),
            names: shared
                .resources
                .iter()
                .map(|r| resource_name(&r.id))
                .collect(),
            early: Vec::with_capacity(limits.ops),
            next_status: 0,
        })
    }
}

fn no_member(system: &str) -> ParamError {
    ParamError::NoIoMember {
        system: system.to_string(),
    }
}

fn os_error(e: OsError) -> ParamError {
    ParamError::Resource {
        id: "io".into(),
        message: e.to_string(),
    }
}

/// Every resource some member is granted, as indices into the config's.
fn wanted(shared: &super::IoShared, members: &[Rc<Member>]) -> Result<Vec<bool>, ParamError> {
    let mut wanted = vec![false; shared.resources.len()];
    for member in members {
        for cap in &member.caps {
            wanted[resource_of(shared, &member.id, cap)? as usize] = true;
        }
    }
    Ok(wanted)
}

/// The index of the resource system `id` granted its parameter `cap`.
fn resource_of(shared: &super::IoShared, id: &str, cap: &str) -> Result<u16, ParamError> {
    let missing = || ParamError::Resource {
        id: format!("{id}.{cap}"),
        message: "no grant".into(),
    };
    let grants = shared.grants.iter().find(|(s, _)| s == id).map(|(_, g)| g);
    let grant = grants
        .and_then(|g| g.iter().find(|g| g.param == cap))
        .ok_or_else(missing)?;
    let at = shared.resources.iter().position(|r| r.id == grant.resource);
    at.map(|at| at as u16).ok_or_else(missing)
}

/// Opens each wanted real resource, in config order.
fn open_all(shared: &super::IoShared, wanted: &[bool]) -> Result<Vec<Option<Opened>>, ParamError> {
    let open_one = |(resource, &wanted): (&super::ResourceConfig, &bool)| {
        let Backing::Real(kind) = &resource.backing else {
            return Ok(None);
        };
        if !wanted {
            return Ok(None);
        }
        let failed = |e: super::OpenError| ParamError::Resource {
            id: resource.id.clone(),
            message: e.to_string(),
        };
        open(kind).map(Some).map_err(failed)
    };
    shared.resources.iter().zip(wanted).map(open_one).collect()
}

fn dial(opened: &Option<Opened>) -> Option<RawAddr> {
    match opened {
        Some(Opened::Dial { addr }) => Some(*addr),
        _ => None,
    }
}

/// Hands each opened descriptor to the `Os`. Returns every resource's `Os`
/// handle: dial target `i` is `RawHandle::new(i, 0)`.
fn register(os: &mut dyn Os, opened: Vec<Option<Opened>>, dials: usize) -> Vec<Option<RawHandle>> {
    let mut next_dial = 0..dials as u32;
    let handle = |opened| match opened {
        Some(Opened::Fd { fd, class }) => Some(os.register(fd, class)),
        Some(Opened::Dial { .. }) => next_dial.next().map(|i| RawHandle::new(i, 0)),
        None => None,
    };
    opened.into_iter().map(handle).collect()
}

fn host_member(
    shared: &super::IoShared,
    reactor: &mut Reactor,
    member: Rc<Member>,
    handles: &[Option<RawHandle>],
) -> Result<HostMember, ParamError> {
    let grants = member
        .caps
        .iter()
        .map(|cap| {
            let res = resource_of(shared, &member.id, cap)?;
            let sim = matches!(shared.resources[res as usize].backing, Backing::Sim(_));
            let os = match sim {
                // A model reads requests by resource and never this handle.
                true => Some(RawHandle::new(u32::from(res), 0)),
                false => handles[res as usize],
            };
            let os = os.ok_or_else(|| no_member(&member.id))?;
            Ok((os, res))
        })
        .collect::<Result<Vec<_>, ParamError>>()?;
    let src = reactor.source(member.table.bufs.clone());
    member.quiesce.src.set(Some(src.0));
    Ok(HostMember {
        member,
        src,
        handles: Handles::new(&grants, MAX_OPS),
        ops: vec![None; MAX_OPS].into(),
    })
}

/// The slot an op's token names, if the op is one its system tracks.
fn tracked(token: super::Token) -> Option<usize> {
    let slot = (token.get() & 0xFFFF) as usize;
    (slot < MAX_OPS).then_some(slot)
}

/// Carries every member's ops to the reactor.
struct SubmitStep {
    io: SharedIo,
    sqs: Vec<Input<Sqe>>,
    log: LogPort,
}

impl Step for SubmitStep {
    fn execute(&mut self, now: Timestamp) {
        let mut io = self.io.borrow_mut();
        let Some(host) = io.host.as_mut() else {
            return;
        };
        let mut corrupt = false;
        for (at, sq) in self.sqs.iter_mut().enumerate() {
            let quiescing = host.members[at].member.quiesce.active.get();
            for sqe in sq.drain() {
                match sqe {
                    Ok(_) if quiescing => {}
                    Ok(sqe) => host.route(at, sqe),
                    Err(_) => corrupt = true,
                }
            }
            if quiescing {
                host.drained(at);
            }
        }
        host.reactor.submit();
        if corrupt {
            self.log
                .fault(now, "io_corrupt", "an SQ record did not decode");
        }
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.log.fault(now, "panic", message.to_string());
    }
}

impl IoHost {
    /// Maps one op's handle and pushes it, or answers it here.
    fn route(&mut self, at: usize, mut sqe: Sqe) {
        let member = &mut self.members[at];
        let Some(mapped) = member.handles.get(sqe.handle) else {
            self.early.push((at, Cqe::err(sqe.token, IoError::Stale)));
            return;
        };
        if let Op::Close { .. } = sqe.op {
            // A grant ends only this capability, and the resource stays open.
            // A derived handle closes under a reactor-own token, which the
            // reactor never refuses.
            member.handles.close(sqe.handle);
            if mapped.derived {
                close(&mut self.reactor, mapped.os);
            }
            self.early.push((at, Cqe::ok(sqe.token, Out::UNIT)));
            return;
        }
        if let Some(slot) = tracked(sqe.token) {
            member.ops[slot] = Some(InFlight {
                res: mapped.res,
                kind: sqe.op.kind(),
            });
            self.counters[mapped.res as usize].submitted += 1;
        }
        sqe.handle = mapped.os;
        self.reactor.push(member.src, sqe);
    }

    /// Notes that a quiescing member's SQ is drained. The first time, closes
    /// its derived handles and revives its grants for the next make.
    fn drained(&mut self, at: usize) {
        let member = &mut self.members[at];
        if member.member.quiesce.drained.replace(true) {
            return;
        }
        let reactor = &mut self.reactor;
        member.handles.drop_derived(|os| close(reactor, os));
        member.handles.revive_grants();
        member.ops.iter_mut().for_each(|op| *op = None);
    }

    /// Counts one completion, maps any handle it opened, and returns it, or
    /// `None` when no system waits for it.
    fn land(&mut self, at: usize, cqe: Cqe) -> Option<Cqe> {
        let member = &mut self.members[at];
        let opened = cqe.status.result().ok().and(cqe.out.as_handle());
        if member.member.quiesce.active.get() {
            // The system that asked is gone, so nothing will close it.
            if let Some(os) = opened {
                close(&mut self.reactor, os);
            }
            return None;
        }
        let op = tracked(cqe.token).and_then(|slot| member.ops[slot].take())?;
        count(&mut self.counters[op.res as usize], op.kind, &cqe);
        let Some(os) = opened else {
            return Some(cqe);
        };
        match member.handles.derive(os, op.res) {
            Some(raw) => Some(Cqe::ok(cqe.token, Out::handle(raw))),
            None => {
                close(&mut self.reactor, os);
                Some(Cqe::err(cqe.token, IoError::Refused))
            }
        }
    }

    /// Writes up to `STATUS_PER_CYCLE` changed resources, starting after the
    /// last one written. A record the ring refuses is written again later.
    fn report(&mut self, now: Timestamp, out: &Output<ResourceStatus>) {
        let mut picked = [0; STATUS_PER_CYCLE];
        let due = due(&self.counters, &self.reported, self.next_status);
        let n = due.zip(&mut picked).map(|(at, slot)| *slot = at).count();
        for &at in &picked[..n] {
            if out
                .write(&self.counters[at].status(now, self.names[at]))
                .is_err()
            {
                return;
            }
            self.reported[at] = self.counters[at];
            self.next_status = (at + 1) % self.counters.len();
        }
    }
}

/// Closes an `Os` handle under a reactor-own token.
fn close(reactor: &mut Reactor, os: RawHandle) {
    reactor.push_own(Sqe::new(Token::new(0), os, Op::CLOSE));
}

/// Counts one finished op: its result, and the bytes it moved.
fn count(counters: &mut Counters, kind: OpKind, cqe: &Cqe) {
    let ok = cqe.status.result().is_ok();
    counters.completed += 1;
    counters.failed += u64::from(!ok);
    let moved = match ok {
        true => u64::from(cqe.out.as_len().unwrap_or(0)),
        false => 0,
    };
    match kind {
        OpKind::Read | OpKind::Recv => counters.bytes_in += moved,
        OpKind::Write | OpKind::Send | OpKind::SendTo => counters.bytes_out += moved,
        _ => {}
    }
}

/// The resources whose counters changed since they were last reported, at
/// most `STATUS_PER_CYCLE`, starting at `start` and wrapping.
pub(crate) fn due<'a>(
    counters: &'a [Counters],
    reported: &'a [Counters],
    start: usize,
) -> impl Iterator<Item = usize> + 'a {
    let count = counters.len();
    (0..count)
        .map(move |step| (start + step) % count)
        .filter(|&at| counters[at] != reported[at])
        .take(STATUS_PER_CYCLE)
}

/// Carries every member's completions back to its CQ.
struct CompleteStep {
    io: SharedIo,
    cqs: Vec<Output<Cqe>>,
    status: Output<ResourceStatus>,
    log: LogPort,
    dropped: Arc<AtomicU32>,
    done: Vec<(SourceId, Cqe)>,
}

impl Step for CompleteStep {
    fn execute(&mut self, now: Timestamp) {
        let mut io = self.io.borrow_mut();
        let Some(host) = io.host.as_mut() else {
            return;
        };
        host.reactor.reap(&mut self.done);
        for (src, cqe) in self.done.drain(..) {
            let at = usize::from(src.0);
            if let (Some(cqe), Some(cq)) = (host.land(at, cqe), self.cqs.get(at)) {
                let _ = cq.write(&cqe);
            }
        }
        for (at, cqe) in host.early.drain(..) {
            if let Some(cq) = self.cqs.get(at) {
                let _ = cq.write(&cqe);
            }
        }
        host.report(now, &self.status);
        report_dropped(&mut self.log, &self.dropped, now);
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.log.fault(now, "panic", message.to_string());
    }
}
