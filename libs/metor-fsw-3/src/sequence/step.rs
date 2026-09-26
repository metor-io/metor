//! The step a sequence is bound as: its future, polled once per cycle.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;

use metor_fsw_3_ring::NoWake;
use metor_proto::types::Timestamp;

use crate::channel::Abort;
use crate::coordinator::{EndReason, Step};
use crate::fn_system::{Cycle, InSet, OutSet, Param};
use crate::log::{self, Log, LogPort};
use crate::port::{Input, Samplers};
use crate::system::{InputBinding, OutputBinding, SystemInputs, SystemOutputs};

use super::{Seq, SeqError, SequenceFn};

/// A sequence's future, owning its state, its ports, and its handle.
type Run = Pin<Box<dyn Future<Output = Result<(), SeqError>>>>;

/// A `SequenceStep` polls one sequence each cycle until it ends.
pub(crate) struct SequenceStep {
    seq: Rc<Seq>,
    abort: Abort,
    samplers: Samplers,
    log: LogPort,
    run: Option<Run>,
    end: Option<EndReason>,
}

impl SequenceStep {
    /// Binds the sequence and builds its future, which runs nothing until polled.
    ///
    /// # Panics
    /// When `inputs` does not start with the `abort` binding [`super::def`] puts first.
    pub(crate) fn new<S: SequenceFn>(
        state: S::State,
        inputs: Vec<InputBinding>,
        outputs: Vec<OutputBinding>,
    ) -> Self {
        let mut inputs = inputs.into_iter();
        // PANIC Safety: `def` declares `abort` first, and build binds one list
        // per declared input; `AbortRequest` needs no alignment.
        let abort = inputs.next().expect("the abort binding");
        let abort = Input::try_new(abort.views).expect("byte alignment");
        let InSet { ins, samplers } = InSet::<S>::bind(inputs.collect());
        let OutSet { outs, log } = OutSet::<S>::bind(outputs);
        let seq = Rc::new(Seq::default());
        Self {
            run: Some(Box::pin(run::<S>(state, ins, outs, seq.clone()))),
            samplers,
            seq,
            abort: Abort::new(abort),
            log,
            end: None,
        }
    }

    /// Keeps the end, logging a failure's reason, which is all a pack's host sees.
    fn finish(&mut self, now: Timestamp, result: Result<(), SeqError>) {
        let end = match result {
            Ok(()) => EndReason::Completed,
            Err(SeqError::Aborted) => EndReason::Aborted,
            Err(SeqError::TimedOut) => EndReason::Failed("timed out".into()),
            Err(SeqError::Failed(reason)) => EndReason::Failed(reason),
        };
        if let EndReason::Failed(reason) = &end {
            self.log.fault(now, "failed", reason.clone());
        }
        self.end = Some(end);
    }
}

/// Hands the sequence its parameters and runs it, all inside one future.
async fn run<S: SequenceFn>(
    mut state: S::State,
    mut ins: <S::Params as Param>::In<NoWake>,
    mut outs: <S::Params as Param>::Out,
    seq: Rc<Seq>,
) -> Result<(), SeqError> {
    let mut log = Log;
    let mut cx = Cycle::with_seq(seq.now(), &mut log, &seq);
    let items = S::Params::get::<NoWake>(&mut ins, &mut outs, &mut cx);
    S::call(&mut state, items).await
}

/// Polls the body once, or drops it, running its handlers, if the sequence is aborted.
fn advance(
    seq: &Seq,
    samplers: &mut Samplers,
    run: &mut Option<Run>,
) -> Poll<Result<(), SeqError>> {
    if seq.aborted() {
        *run = None;
        return Poll::Ready(Err(SeqError::Aborted));
    }
    samplers.take_all();
    let Some(body) = run.as_mut() else {
        return Poll::Pending;
    };
    let _polling = seq.polling();
    body.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

impl Step for SequenceStep {
    fn execute(&mut self, now: Timestamp) {
        self.seq.set_now(now);
        if self.abort.requested() {
            let _ = self.seq.abort();
        }
        if self.run.is_none() {
            return;
        }
        let Self {
            seq,
            samplers,
            log,
            run,
            ..
        } = self;
        let polled = log::with_log_port(log, now, || advance(seq, samplers, run));
        if let Poll::Ready(result) = polled {
            self.run = None;
            self.finish(now, result);
        }
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.log.fault(now, "panic", message.to_string());
        let _ = self.seq.abort();
        let run = &mut self.run;
        log::with_log_port(&mut self.log, now, || drop(run.take()));
    }

    fn ended(&self) -> Option<&EndReason> {
        self.end.as_ref()
    }
}
