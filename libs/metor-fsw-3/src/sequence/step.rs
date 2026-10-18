//! The step that runs a sequence.

use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::AtomicU32;
use core::task::Poll;
use std::rc::Rc;
use std::sync::Arc;

use metor_fsw_3_ring::NoWake;
use metor_proto::types::Timestamp;

use crate::channel::{Abort, AbortRequest};
use crate::coordinator::{EndReason, Step};
use crate::fn_system::{Cycle, InSet, OutSet, Param};
use crate::log::{self, LogPort};
use crate::port::{Input, Samplers};

use super::{Seq, SeqError, SequenceFn};

/// A sequence's body, boxed as a future.
type Run = Pin<Box<dyn Future<Output = Result<(), SeqError>>>>;

/// Where a sequence's body is.
enum Body {
    Running(Run),
    Ended(EndReason),
}

/// Runs one sequence, polling its body once per cycle until it ends.
pub(crate) struct SequenceStep {
    seq: Rc<Seq<'static>>,
    abort: Abort,
    samplers: Samplers,
    log: LogPort,
    /// Counts failed writes on the outputs the body owns.
    dropped: Arc<AtomicU32>,
    body: Body,
}

impl SequenceStep {
    /// Creates the sequence's body over its bound ports.
    ///
    /// The body does not start running until it is first polled.
    pub(crate) fn new<S: SequenceFn>(
        state: S::State,
        abort: Input<AbortRequest>,
        inputs: InSet<S>,
        outputs: OutSet<S>,
    ) -> Self {
        let InSet { ins, samplers } = inputs;
        let OutSet { outs, log, dropped } = outputs;
        let seq = Rc::new(Seq::new());
        Self {
            body: Body::Running(Box::pin(run::<S>(state, ins, outs, seq.clone()))),
            samplers,
            seq,
            abort: Abort::new(abort),
            log,
            dropped,
        }
    }

    /// How a body that returned `result` ended.
    fn end_reason(result: Result<(), SeqError>) -> EndReason {
        match result {
            Ok(()) => EndReason::Completed,
            Err(SeqError::Aborted) => EndReason::Aborted,
            Err(SeqError::TimedOut) => EndReason::Failed("timed out".into()),
            Err(SeqError::Failed(reason)) => EndReason::Failed(reason),
        }
    }
}

/// Runs the sequence's body.
///
/// The parameters are created inside the same future, so they can borrow
/// the state and ports it holds across `.await` points.
async fn run<S: SequenceFn>(
    mut state: S::State,
    mut ins: <S::Params as Param>::In<NoWake>,
    mut outs: <S::Params as Param>::Out,
    seq: Rc<Seq<'static>>,
) -> Result<(), SeqError> {
    let mut cx = Cycle::with_seq(seq.now(), &seq);
    let items = S::Params::get::<NoWake>(&mut ins, &mut outs, &mut cx);
    S::call(&mut state, items).await
}

impl Step for SequenceStep {
    fn execute(&mut self, now: Timestamp) {
        self.seq.set_now(now);
        if self.abort.requested() {
            let _ = self.seq.abort();
        }
        let Self {
            seq,
            samplers,
            log,
            body,
            ..
        } = self;
        let Body::Running(run) = body else {
            return;
        };
        let polled = log::with_log_port(log, now, || {
            if !seq.aborted() {
                samplers.take_all();
            }
            seq.advance(run.as_mut())
        });
        let finished = polled.is_ready();
        if let Poll::Ready(result) = polled {
            let done = core::mem::replace(body, Body::Ended(Self::end_reason(result)));
            log::with_log_port(log, now, || drop(done));
            *samplers = Samplers::default();
        }
        log.report_failed_writes(&self.dropped, now);
        // A failure's reason is logged too, since that line is the only way a
        // pack's host sees it.
        if finished && let Body::Ended(EndReason::Failed(reason)) = &self.body {
            self.log.fault(now, "failed", reason.clone());
        }
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.log.fault(now, "panic", message.to_string());
        let _ = self.seq.abort();
        let done = core::mem::replace(&mut self.body, Body::Ended(EndReason::Panicked));
        log::with_log_port(&mut self.log, now, || drop(done));
        self.log.report_failed_writes(&self.dropped, now);
    }

    fn ended(&self) -> Option<&EndReason> {
        match &self.body {
            Body::Running(_) => None,
            Body::Ended(end) => Some(end),
        }
    }
}
