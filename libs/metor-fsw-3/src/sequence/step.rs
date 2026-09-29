//! The step that runs a sequence.

use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::AtomicU32;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;
use std::sync::Arc;

use metor_fsw_3_ring::NoWake;
use metor_proto::types::Timestamp;

use crate::channel::Abort;
use crate::coordinator::{EndReason, Step};
use crate::fn_system::{Cycle, InSet, OutSet, Param, report_dropped};
use crate::log::{self, Log, LogPort};
use crate::port::{Input, Samplers};
use crate::system::{InputBinding, OutputBinding, SystemInputs, SystemOutputs};

use super::{Seq, SeqError, SequenceFn};

/// A sequence's body, boxed as a future.
type Run = Pin<Box<dyn Future<Output = Result<(), SeqError>>>>;

/// Runs one sequence, polling its body once per cycle until it ends.
pub(crate) struct SequenceStep {
    seq: Rc<Seq<'static>>,
    abort: Abort,
    samplers: Samplers,
    log: LogPort,
    /// Counts failed writes on the outputs the body owns.
    dropped: Arc<AtomicU32>,
    run: Option<Run>,
    end: Option<EndReason>,
}

impl SequenceStep {
    /// Binds the sequence's ports and creates its body.
    ///
    /// The body does not start running until it is first polled.
    ///
    /// # Panics
    /// Panics if `inputs` does not start with the `abort` binding, which
    /// [`super::def`] always declares first.
    pub(crate) fn new<S: SequenceFn>(
        state: S::State,
        inputs: Vec<InputBinding>,
        outputs: Vec<OutputBinding>,
    ) -> Self {
        let mut inputs = inputs.into_iter();
        // PANIC Safety: `def` declares `abort` first, and build binds one list
        // per declared input.
        let abort = inputs.next().expect("the abort binding");
        let abort = Input::new(abort.views);
        let InSet { ins, samplers } = InSet::<S>::bind(inputs.collect());
        let OutSet { outs, log, dropped } = OutSet::<S>::bind(outputs);
        let seq = Rc::new(Seq::new());
        Self {
            run: Some(Box::pin(run::<S>(state, ins, outs, seq.clone()))),
            samplers,
            seq,
            abort: Abort::new(abort),
            log,
            dropped,
            end: None,
        }
    }

    /// Records how the sequence ended.
    ///
    /// A failure's reason is also logged as a fault, since that log line is
    /// the only way a pack's host sees it.
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
    let mut log = Log;
    let mut cx = Cycle::with_seq(seq.now(), &mut log, &seq);
    let items = S::Params::get::<NoWake>(&mut ins, &mut outs, &mut cx);
    S::call(&mut state, items).await
}

/// Polls the body once, or drops it if the sequence should abort.
///
/// The body is dropped if the sequence was aborted or a redline trips.
/// Dropping it runs its abort handlers.
///
/// Redlines are only checked while the body is alive, because the body owns
/// everything their conditions borrow.
fn advance(
    seq: &Seq<'_>,
    samplers: &mut Samplers,
    run: &mut Option<Run>,
) -> Poll<Result<(), SeqError>> {
    let Some(body) = run.as_mut() else {
        return Poll::Pending;
    };
    if !seq.aborted() {
        samplers.take_all();
        trip(seq);
    }
    if seq.aborted() {
        *run = None;
        return Poll::Ready(Err(SeqError::Aborted));
    }
    let _polling = seq.polling();
    body.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

/// Aborts the sequence if any redline trips.
///
/// Logs a `redline` fault with the name of the redline that tripped.
fn trip(seq: &Seq<'_>) {
    if let Some(name) = seq.tripped() {
        Log.fault("redline", name);
        let _ = seq.abort();
    }
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
        report_dropped(log, &self.dropped, now);
        if let Poll::Ready(result) = polled {
            self.run = None;
            self.samplers = Samplers::default();
            self.finish(now, result);
        }
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.log.fault(now, "panic", message.to_string());
        let _ = self.seq.abort();
        let run = &mut self.run;
        log::with_log_port(&mut self.log, now, || drop(run.take()));
        report_dropped(&mut self.log, &self.dropped, now);
    }

    fn ended(&self) -> Option<&EndReason> {
        self.end.as_ref()
    }
}
