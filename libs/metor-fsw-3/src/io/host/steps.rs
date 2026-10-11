//! `fsw.io`: the system that carries every IO system's ops to the backend or
//! a wire, and their completions back.
//!
//! A target places it twice, as `io_submit` after the drivers and as
//! `io_complete` after the models. Both instances share one dispatch and
//! backend, and run the same body: lower the SQs of the members that ran
//! since the previous instance, hand the ops to the backend, reap it, and
//! post the ready completions of the members whose CQ the instance writes.
//! A member's SQ goes to the next instance after it, so one must follow it,
//! and its CQ comes from the one before it, wrapping to the last for members
//! placed before the first. So a driver's request reaches its model in the
//! cycle it is sent, and the model's answer reaches the driver in the next.
//!
//! A pass lowers at most two records per slot from each SQ, and a
//! completion its CQ refuses is held for the next pass, up to a bound.

use std::borrow::Cow;
use std::rc::Rc;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;
use schemars::JsonSchema;
use serde::Deserialize;

use super::dispatch::Dispatch;
use super::{Host, Os, OsDone, Route};
use crate::coordinator::{
    Adapters, MakeCx, ParamError, Step, SystemKind, TableEntry,
};
use crate::def::{DefCx, DefError};
use crate::fn_system::LOG_PORT;
use crate::io::arena::Arena;
use crate::io::status::ResourceStatus;
use crate::io::{Cqe, IoShared, MAX_OPS, SharedIo, Sqe};
use crate::log::LogPort;
use crate::port::{Input, Output, dyn_input_defs, dyn_output_defs};
use crate::system::SystemDef;

pub const IO_TYPE: &str = "fsw.io";
/// The output the instance with `status` writes resource counts to.
pub const IO_STATUS_PORT: &str = "io_status";
/// The most SQ records lowered from one member per pass: an op and a cancel
/// for each slot. The rest wait for the next pass.
const SQ_PER_PASS: usize = 2 * MAX_OPS;

/// The params of one `fsw.io` instance.
#[derive(Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
struct IoParams {
    /// Whether this instance writes `io_status`. A target sets it on one.
    #[serde(default)]
    status: bool,
}

pub(crate) fn entry() -> TableEntry {
    TableEntry {
        kind: SystemKind::Cyclic,
        adapters: Adapters::BARE,
        // PANIC Safety: the empty context names no ports, which cannot fail.
        def: def(&DefCx::empty()).expect("a static definition"),
        def_fn: Box::new(def),
        doc: "Carries IO systems' ops to the backend or a wire, and their completions back.".into(),
        schema: serde_json::value::to_raw_value(&schemars::schema_for!(IoParams)).ok(),
        make: Arc::new(make),
    }
}

/// One SQ input per member it lowers, named after the member; `log`,
/// `io_status`, then one CQ per member it posts to, named after it.
fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let fixed = [Cow::Borrowed(LOG_PORT), Cow::Borrowed(IO_STATUS_PORT)];
    let mut outputs = vec![
        Output::<LogEvent>::def(cx, LOG_PORT),
        Output::<ResourceStatus>::def(cx, IO_STATUS_PORT),
    ];
    outputs.extend(dyn_output_defs(cx, &fixed)?);
    Ok(SystemDef {
        name: "io".into(),
        inputs: dyn_input_defs(cx, &[])?,
        outputs,
        grants: Vec::new(),
    })
}

fn make(mut cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError> {
    let params: IoParams = cx.params.decode()?;
    let (views, writers) = cx.take_bindings();
    let io = cx.io.borrow();
    let names = cx.def.inputs.iter().map(|p| p.name.as_ref());
    let members: Vec<usize> = names.map(|n| member(&io, n)).collect::<Result<_, _>>()?;
    let sqs = members.into_iter().zip(views);
    let mut writers = writers.into_iter();
    // PANIC Safety: `def` declares `log`, then `io_status`, first.
    let mut fixed = || writers.next().expect("a fixed output").writer;
    let log = LogPort::new(Output::new(fixed()));
    let status = Output::new(fixed());
    let mut cqs: Vec<Option<Output<Cqe>>> = io.members.iter().map(|_| None).collect();
    for b in writers {
        let m = member(&io, &b.def.name)?;
        cqs[m] = Some(Output::new(b.writer));
    }
    let owned = cqs.iter().flatten().count() * MAX_OPS;
    let ops = io.members.len() * MAX_OPS;
    Ok(Box::new(IoStep {
        io: cx.io.clone(),
        sqs: sqs.map(|(m, b)| (m, Input::new(b.views))).collect(),
        owns: cqs.iter().map(Option::is_some).collect(),
        cqs,
        status: params.status.then_some(status),
        log,
        done: Vec::with_capacity(2 * ops),
        held: 2 * owned,
        ready: Vec::with_capacity(2 * owned + 3 * ops),
    }))
}

fn member(io: &IoShared, id: &str) -> Result<usize, ParamError> {
    let system = id.to_string();
    io.index(id).ok_or(ParamError::NoIoMember { system })
}

/// One placement of `fsw.io`.
struct IoStep {
    io: SharedIo,
    /// The SQ of each member it lowers, with the member's index.
    sqs: Vec<(usize, Input<Sqe>)>,
    /// The CQ of each member it posts to, by member index.
    cqs: Vec<Option<Output<Cqe>>>,
    owns: Box<[bool]>,
    status: Option<Output<ResourceStatus>>,
    log: LogPort,
    done: Vec<OsDone>,
    /// The most completions a full CQ holds back for the next pass.
    held: usize,
    /// Completions to post, oldest first, starting with those a full CQ
    /// held back.
    ready: Vec<(usize, Cqe)>,
}

/// What went wrong in one pass, reported as faults.
#[derive(Default)]
struct Faults {
    corrupt: bool,
    rejected: bool,
    lost: bool,
}

impl Step for IoStep {
    fn execute(&mut self, now: Timestamp) {
        let shared = self.io.clone();
        let mut io = shared.borrow_mut();
        let IoShared { members, host, .. } = &mut *io;
        let Some(Host { dispatch, os }) = host.as_mut() else {
            return;
        };
        let mut faults = self.lower(dispatch, members);
        self.run(dispatch, os.as_mut());
        dispatch.take_ready(&self.owns, &mut self.ready);
        faults.lost = self.post();
        if let Some(status) = &self.status {
            dispatch.report(now, status);
        }
        self.report(now, faults);
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.log.fault(now, "panic", message.to_string());
    }
}

impl IoStep {
    /// Lowers what each member's SQ holds, and notes the quiescing members
    /// whose SQ is drained.
    fn lower(&mut self, dispatch: &mut Dispatch, members: &[(String, Rc<Arena>)]) -> Faults {
        let mut faults = Faults::default();
        for (m, sq) in &mut self.sqs {
            for sqe in sq.drain().take(SQ_PER_PASS) {
                match sqe {
                    Ok(sqe) => {
                        faults.rejected |= dispatch.lower(*m, sqe, &members[*m].1).is_err();
                    }
                    Err(_) => faults.corrupt = true,
                }
            }
            dispatch.member_drained(*m);
        }
        faults
    }

    /// Carries the routes out on the backend, then raises what it finished.
    fn run(&mut self, dispatch: &mut Dispatch, os: &mut dyn Os) {
        for route in dispatch.drain_routes() {
            match route {
                Route::Os(op) => os.push(op),
                Route::Cancel(id) => os.cancel(id),
                Route::Close(fd) => os.close(fd),
            }
        }
        os.submit();
        os.reap(&mut self.done);
        for done in self.done.drain(..) {
            dispatch.raise(done);
        }
    }

    /// Posts the ready completions. One a full CQ refuses is held for the
    /// next pass, up to `held`; past that it is lost: `true`.
    fn post(&mut self) -> bool {
        let (cqs, held) = (&self.cqs, self.held);
        let (mut kept, mut lost) = (0, false);
        self.ready.retain(|(m, cqe)| {
            let Some(Some(cq)) = cqs.get(*m) else {
                return false;
            };
            if cq.write(cqe).is_ok() {
                return false;
            }
            kept += 1;
            lost |= kept > held;
            kept <= held
        });
        lost
    }

    fn report(&mut self, now: Timestamp, faults: Faults) {
        if faults.rejected {
            let message = "an SQ record named a slot that is not free";
            self.log.fault(now, "io_rejected", message);
        }
        if faults.corrupt {
            self.log
                .fault(now, "io_corrupt", "an SQ record did not decode");
        }
        if faults.lost {
            let message = "a CQ stayed full, so a completion was lost";
            self.log.fault(now, "io_cq_full", message);
        }
    }
}

#[cfg(test)]
mod tests;
