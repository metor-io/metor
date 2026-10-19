//! `fsw.io`: lowers its members' SQs to the backend or a wire, and posts
//! their completions to their CQs.

use std::borrow::Cow;
use std::rc::Rc;
use std::sync::Arc;

use metor_fsw_3_ring::{NoWake, RingBuffer};

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;
use schemars::JsonSchema;
use serde::Deserialize;

use super::dispatch::Dispatch;
use super::{Host, MemberQueue, Os, OsDone, Route};
use crate::coordinator::{Adapters, MakeCx, MakeError, Step, StepCx, SystemKind, TableEntry};
use crate::def::{DefCx, DefError};
use crate::fn_system::LOG_PORT;
use crate::io::arena::Arena;
use crate::io::status::ResourceStatus;
use crate::io::{IoState, MAX_OPS};
use crate::log::LogPort;
use crate::port::Output;
use crate::system::{PortDef, SystemDef};

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
    outputs.extend(cx.dyn_output_defs(&fixed)?);
    Ok(SystemDef {
        name: "io".into(),
        inputs: cx.dyn_input_defs(&[])?,
        outputs,
        log: Some(0),
        grants: Vec::new(),
        io: None,
    })
}

fn make(cx: MakeCx<'_>) -> Result<Box<dyn Step>, MakeError> {
    let params: IoParams = cx.params.decode()?;
    let MakeCx {
        def,
        inputs,
        outputs,
        io,
        ..
    } = cx;
    let index = |ports: &[PortDef]| -> Result<Vec<usize>, MakeError> {
        ports.iter().map(|p| io.member_index(&p.name)).collect()
    };
    let lowers = index(&def.inputs)?;
    // PANIC Safety: `def` declares `log`, then `io_status`, before the CQs.
    let [log, status, cq_rings @ ..] = outputs.as_slice() else {
        panic!("`def` declares two fixed outputs");
    };
    let cq_members = index(&def.outputs[2..])?;
    let mut posts = vec![false; io.members.len()];
    for &m in &cq_members {
        posts[m] = true;
    }
    let ops = io.members.len() * MAX_OPS;
    if let Some(host) = &mut io.host {
        host.bind_sqs(lowers.iter().copied().zip(inputs));
        host.bind_cqs(cq_members.iter().copied().zip(cq_rings.iter().copied()));
    }
    Ok(Box::new(IoStep {
        held: Dispatch::held_for(cq_members.len()),
        lowers,
        posts,
        status: params.status.then(|| writer(status)),
        log: LogPort::new(writer(log)),
        done: Vec::with_capacity(2 * ops),
    }))
}

fn writer<T: crate::Record + ?Sized>(ring: &RingBuffer) -> Output<T> {
    // PANIC Safety: build allocates one ring per output.
    Output::new(ring.writer(NoWake).expect("one writer per output ring"))
}

/// One placement of `fsw.io`. The queues it serves live in `Host`.
struct IoStep {
    /// The members whose SQ it lowers.
    lowers: Vec<usize>,
    /// Whether it posts to each member's CQ, by member index.
    posts: Vec<bool>,
    status: Option<Output<ResourceStatus>>,
    log: LogPort,
    done: Vec<OsDone>,
    /// The most completions a full CQ holds back for the next pass.
    held: usize,
}

/// What went wrong in one pass, reported as faults.
#[derive(Default)]
struct Faults {
    corrupt: bool,
    rejected: bool,
    lost: bool,
}

impl Step for IoStep {
    fn execute(&mut self, now: Timestamp, cx: &mut StepCx<'_>) {
        let IoState { members, host, .. } = &mut *cx.io;
        let Some(Host {
            dispatch,
            os,
            queues,
        }) = host.as_mut()
        else {
            return;
        };
        let mut faults = self.lower(dispatch, queues, members);
        self.run(dispatch, os.as_mut());
        let posts = &self.posts;
        faults.lost = dispatch.post_ready(self.held, |m, cqe| {
            posts.get(m).copied().unwrap_or(false).then_some(())?;
            let cq = queues.get_mut(m)?.cq.as_mut()?;
            Some(cq.write(cqe).is_ok())
        });
        if let Some(status) = &mut self.status {
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
    fn lower(
        &mut self,
        dispatch: &mut Dispatch,
        queues: &mut [MemberQueue],
        members: &[(String, Rc<Arena>)],
    ) -> Faults {
        let mut faults = Faults::default();
        for &m in &self.lowers {
            let Some(sq) = queues.get_mut(m).and_then(|q| q.sq.as_mut()) else {
                continue;
            };
            for sqe in sq.drain().take(SQ_PER_PASS) {
                match sqe {
                    Ok(sqe) => {
                        faults.rejected |= dispatch.lower(m, sqe, &members[m].1).is_err();
                    }
                    Err(_) => faults.corrupt = true,
                }
            }
            dispatch.member_drained(m);
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
