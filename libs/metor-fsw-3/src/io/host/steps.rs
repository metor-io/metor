//! `io_submit` and `io_complete`: the two steps that carry every IO system's
//! ops to the backend or a model, and their completions back.
//!
//! `io_submit` runs after every IO system. It lowers each member's SQ
//! through the dispatch and hands the ops to the backend, or writes a sim
//! op to its resource's `{resource}.req` output. `io_complete` runs later
//! in the cycle: it reaps the backend, reads the models' `{resource}.rep`
//! replies, and writes each completion to its member's CQ, which the system
//! reads on the next cycle. It also writes `io_status`.

use core::sync::atomic::AtomicU32;
use std::borrow::Cow;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use super::{Backed, Host, OsDone, Route};
use crate::coordinator::{
    MakeCx, ParamError, Step, SystemKind, TableEntry, bindings, refuse_thread,
};
use crate::def::{DefCx, DefError};
use crate::fn_system::{LOG_PORT, report_dropped};
use crate::io::status::ResourceStatus;
use crate::io::{Cqe, IoError, IoShared, MAX_OPS, Op, SharedIo, Sqe};
use crate::log::LogPort;
use crate::port::{Input, Output, dyn_input_defs, dyn_output_defs};
use crate::system::{OutputBinding, SystemDef};

pub const IO_SUBMIT_TYPE: &str = "fsw.io_submit";
pub const IO_COMPLETE_TYPE: &str = "fsw.io_complete";
/// The output `io_complete` writes resource counts to.
pub const IO_STATUS_PORT: &str = "io_status";

pub(crate) fn submit_entry() -> TableEntry {
    TableEntry {
        kind: SystemKind::Cyclic,
        // PANIC Safety: the empty context names no ports, which cannot fail.
        def: submit_def(&DefCx::empty()).expect("a static definition"),
        def_fn: Box::new(submit_def),
        doc: "Carries every IO system's ops to the backend.".into(),
        schema: None,
        make: Arc::new(make_submit),
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

/// One input per member, named after it; `log`, then one `{resource}.req`
/// output per sim resource.
fn submit_def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let mut outputs = vec![Output::<LogEvent>::def(cx, LOG_PORT)];
    outputs.extend(dyn_output_defs(cx, &[Cow::Borrowed(LOG_PORT)])?);
    Ok(SystemDef {
        name: "io_submit".into(),
        inputs: dyn_input_defs(cx, &[])?,
        outputs,
        grants: Vec::new(),
    })
}

/// One `{resource}.rep` input per sim resource; `log`, `io_status`, then one
/// CQ per member, named after it.
fn complete_def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let fixed = [Cow::Borrowed(LOG_PORT), Cow::Borrowed(IO_STATUS_PORT)];
    let mut outputs = vec![
        Output::<LogEvent>::def(cx, LOG_PORT),
        Output::<ResourceStatus>::def(cx, IO_STATUS_PORT),
    ];
    outputs.extend(dyn_output_defs(cx, &fixed)?);
    Ok(SystemDef {
        name: "io_complete".into(),
        inputs: dyn_input_defs(cx, &[])?,
        outputs,
        grants: Vec::new(),
    })
}

fn make_submit(cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError> {
    refuse_thread(cx.adapter)?;
    let (views, mut writers) = bindings(cx.def, cx.inputs, cx.outputs);
    let io = cx.io.borrow();
    let names = cx.def.inputs.iter().map(|p| p.name.as_ref());
    let members: Vec<usize> = names.map(|n| member(&io, n)).collect::<Result<_, _>>()?;
    let reqs = writers.split_off(1);
    let reqs = reqs.into_iter().map(|b| (b.def.name.clone(), b));
    let reqs = by_resource(&io, reqs, ".req")?;
    Ok(Box::new(SubmitStep {
        io: cx.io.clone(),
        sqs: members
            .into_iter()
            .zip(views)
            .map(|(m, b)| (m, Input::new(b.views)))
            .collect(),
        reqs: reqs
            .into_iter()
            .map(|p| p.map(|b| Output::new(b.writer)))
            .collect(),
        refused: Vec::with_capacity(io.members.len() * MAX_OPS),
        log: log_port(writers),
    }))
}

fn make_complete(cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError> {
    refuse_thread(cx.adapter)?;
    let io = cx.io.borrow();
    let (views, writers) = bindings(cx.def, cx.inputs, cx.outputs);
    let reps = views.into_iter().map(|b| (b.def.name.clone(), b));
    let reps = by_resource(&io, reps, ".rep")?;
    let mut writers = writers.into_iter();
    let log = log_port(writers.by_ref().take(1).collect());
    // PANIC Safety: `complete_def` declares `io_status` second.
    let status = Output::new(writers.next().expect("the io_status output").writer);
    let dropped = Arc::<AtomicU32>::default();
    let mut cqs: Vec<Option<Output<Cqe>>> = io.members.iter().map(|_| None).collect();
    for b in writers {
        let m = member(&io, &b.def.name)?;
        cqs[m] = Some(Output::counting(b.writer, dropped.clone()));
    }
    Ok(Box::new(CompleteStep {
        io: cx.io.clone(),
        reps: reps
            .into_iter()
            .flatten()
            .map(|b| Input::new(b.views))
            .collect(),
        cqs,
        status,
        log,
        dropped,
        done: Vec::with_capacity(2 * io.members.len() * MAX_OPS),
    }))
}

fn member(io: &IoShared, id: &str) -> Result<usize, ParamError> {
    let system = id.to_string();
    io.index(id).ok_or(ParamError::NoIoMember { system })
}

/// The `log` output, the only writer in `writers`.
fn log_port(writers: Vec<OutputBinding>) -> LogPort {
    // PANIC Safety: both io steps declare `log` first among their outputs.
    let log = writers.into_iter().next().expect("the log output");
    LogPort::new(Output::new(log.writer))
}

/// Ports named `{resource}{suffix}`, by resource index. Each must name a sim
/// resource, and each sim resource a driver holds must have one.
fn by_resource<P>(
    io: &IoShared,
    ports: impl Iterator<Item = (Cow<'static, str>, P)>,
    suffix: &str,
) -> Result<Vec<Option<P>>, ParamError> {
    let resources = io.host.as_ref().map_or(&[][..], |h| h.dispatch.resources());
    let mut found: Vec<Option<P>> = resources.iter().map(|_| None).collect();
    for (name, port) in ports {
        let id = name.strip_suffix(suffix);
        let at = id.and_then(|id| resources.iter().position(|r| r.sim && r.id == id));
        let Some(at) = at else {
            let message = "names no sim resource".into();
            return Err(ParamError::Resource {
                id: name.into(),
                message,
            });
        };
        found[at] = Some(port);
    }
    let held = |at: usize| matches!(resources[at].backed, Backed::Sim(_));
    if let Some(at) = (0..found.len()).find(|&at| held(at) && found[at].is_none()) {
        return Err(ParamError::Resource {
            id: format!("{}{suffix}", resources[at].id),
            message: "a sim resource's model has no edge to the io steps".into(),
        });
    }
    Ok(found)
}

/// Carries every member's ops to the backend, or to a sim resource's model.
struct SubmitStep {
    io: SharedIo,
    /// Each member's SQ, with its member index.
    sqs: Vec<(usize, Input<Sqe>)>,
    /// Each resource's model requests, for a sim resource.
    reqs: Vec<Option<Output<Sqe>>>,
    /// Sim ops and cancels whose model port was full.
    refused: Vec<Route>,
    log: LogPort,
}

impl Step for SubmitStep {
    fn execute(&mut self, now: Timestamp) {
        let mut io = self.io.borrow_mut();
        let IoShared { members, host } = &mut *io;
        let Some(Host { dispatch, os }) = host.as_mut() else {
            return;
        };
        let mut corrupt = false;
        for (m, sq) in &mut self.sqs {
            for sqe in sq.drain() {
                match sqe {
                    Ok(sqe) => dispatch.lower(*m, sqe, &members[*m].1),
                    Err(_) => corrupt = true,
                }
            }
            if dispatch.is_member_queisicing(*m) {
                dispatch.member_drained(*m);
            }
        }
        for route in dispatch.drain_routes() {
            match route {
                Route::Os(op) => os.push(op),
                Route::Cancel(id) => os.cancel(id),
                Route::Close(fd) => os.close(fd),
                Route::Sim(res, sqe) if !write(&self.reqs, res, &sqe) => {
                    self.refused.push(Route::Sim(res, sqe));
                }
                Route::Sim(..) => {}
            }
        }
        for route in self.refused.drain(..) {
            match route {
                Route::Sim(_, sqe) if !matches!(sqe.op(), Ok(Op::Cancel)) => {
                    dispatch.refuse_sim_op(sqe.token, IoError::QueueFull);
                }
                cancel => dispatch.defer(cancel),
            }
        }
        os.submit();
        if corrupt {
            self.log
                .fault(now, "io_corrupt", "an SQ record did not decode");
        }
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.log.fault(now, "panic", message.to_string());
    }
}

/// Writes a sim op to resource `res`'s model. False if its port is full.
fn write(reqs: &[Option<Output<Sqe>>], res: u16, sqe: &Sqe) -> bool {
    let req = reqs.get(usize::from(res)).and_then(Option::as_ref);
    req.is_some_and(|req| req.write(sqe).is_ok())
}

/// Carries every member's completions back to its CQ.
struct CompleteStep {
    io: SharedIo,
    /// The models' replies, one input per sim resource.
    reps: Vec<Input<Cqe>>,
    /// Each member's CQ, by member index.
    cqs: Vec<Option<Output<Cqe>>>,
    status: Output<ResourceStatus>,
    log: LogPort,
    dropped: Arc<AtomicU32>,
    done: Vec<OsDone>,
}

impl Step for CompleteStep {
    fn execute(&mut self, now: Timestamp) {
        let mut io = self.io.borrow_mut();
        let Some(Host { dispatch, os }) = io.host.as_mut() else {
            return;
        };
        let post = |cqs: &[Option<Output<Cqe>>], (m, cqe): (usize, Cqe)| {
            if let Some(Some(cq)) = cqs.get(m) {
                let _ = cq.write(&cqe);
            }
        };
        os.reap(&mut self.done);
        for done in self.done.drain(..) {
            if let Some(reply) = dispatch.raise(done) {
                post(&self.cqs, reply);
            }
        }
        for rep in &mut self.reps {
            for cqe in rep.drain().flatten() {
                if let Some(reply) = dispatch.raise_sim(cqe) {
                    post(&self.cqs, reply);
                }
            }
        }
        for reply in dispatch.replies() {
            post(&self.cqs, reply);
        }
        dispatch.report(now, &self.status);
        report_dropped(&mut self.log, &self.dropped, now);
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.log.fault(now, "panic", message.to_string());
    }
}
