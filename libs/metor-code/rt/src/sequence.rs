//! The step of a compiled sequence.
//!
//! The compiled `resume` is wrapped in a body for fsw-3's sequence step, so
//! the abort input, redlines, `on_abort` guards, the channel adapter and its
//! clocks are fsw-3's own. Each cycle the step copies the inputs in, checks
//! the redlines, then the body calls `resume` once and yields until the next
//! cycle. A failure or timeout aborts first, so `on abort:` runs, then ends
//! the sequence `Failed`.

use core::cell::RefCell;
use core::pin::Pin;
use std::borrow::Cow;
use std::rc::Rc;
use std::sync::Arc;

use metor_fsw_3::log::LogPort;
use metor_fsw_3::sequence::{Run, dynamic_step};
use metor_fsw_3::{Input, Redline, Seq, SeqError, Step};

use crate::cx::ResumeWord;
use crate::instance::Instance;
use crate::ports::Rings;
use crate::report::{AlertEdges, log_faults_and_alerts};
use metor_fsw_3::AbortRequest;

/// What the step's refresh, redlines, body and abort handler share.
struct Core {
    instance: Instance,
    rings: Rings,
    edges: AlertEdges,
}

/// Makes the step of a compiled sequence.
///
/// `redlines` are the names of the type's redlines, in order.
pub fn sequence_step(
    instance: Instance,
    rings: Rings,
    abort: Input<AbortRequest>,
    log: LogPort,
    redlines: Arc<[&'static str]>,
) -> Box<dyn Step> {
    let dropped = rings.dropped.clone();
    let core = Rc::new(RefCell::new(Core {
        edges: AlertEdges::new(&instance),
        instance,
        rings,
    }));
    let refresh = {
        let core = core.clone();
        move || with_core(&core, |c| c.rings.refresh(&mut c.instance))
    };
    dynamic_step(abort, log, dropped, refresh, move |seq| {
        body(core, seq, redlines)
    })
}

/// Runs `f` on the core, unless it is already borrowed.
fn with_core<R: Default>(core: &RefCell<Core>, f: impl FnOnce(&mut Core) -> R) -> R {
    core.try_borrow_mut()
        .map(|mut c| f(&mut c))
        .unwrap_or_default()
}

/// The sequence's body: links its redlines and abort handler, then resumes
/// the compiled code once per cycle until it ends.
fn body(core: Rc<RefCell<Core>>, seq: Rc<Seq<'static>>, names: Arc<[&'static str]>) -> Run {
    Box::pin(async move {
        let handler = core.clone();
        let _on_abort = seq.on_abort(move |seq| {
            with_core(&handler, |c| abort_cleanup(c, seq.now().0));
        });
        let redlines: Pin<Box<[_]>> = names
            .iter()
            .enumerate()
            .map(|(index, &name)| {
                let (core, seq) = (core.clone(), seq.clone());
                Redline::new(name, move || {
                    with_core(&core, |c| c.instance.redline(index, seq.now().0))
                })
            })
            .collect::<Vec<_>>()
            .into_boxed_slice()
            .into();
        for redline in redlines.iter() {
            // SAFETY: the slice is pinned in its box and never moved, so each
            // element stays pinned until the box drops.
            seq.redline(unsafe { Pin::new_unchecked(redline) });
        }
        loop {
            let now = seq.now().0;
            match with_core(&core, |c| resume(c, now)) {
                Some(ResumeWord::Running) | None => {
                    seq.next_cycle().await;
                }
                Some(ResumeWord::Completed) => return Ok(()),
                Some(ResumeWord::Failed) => {
                    let message = with_core(&core, |c| c.instance.fail_message().to_string());
                    let _ = seq.abort();
                    return Err(SeqError::Failed(Cow::Owned(message)));
                }
            }
        }
    })
}

/// Resumes the compiled body once and reports what it did.
///
/// A failed cycle leaves its outputs to the abort handler, which writes them
/// once, after `on abort:` runs.
fn resume(core: &mut Core, now: i64) -> Option<ResumeWord> {
    let word = core.instance.step(now);
    if word != ResumeWord::Failed {
        core.rings.flush(&mut core.instance, now);
    }
    log_faults_and_alerts(&mut core.instance, &mut core.edges);
    Some(word)
}

/// Runs `on abort:` and writes what it and the body left marked.
fn abort_cleanup(core: &mut Core, now: i64) {
    core.instance.on_abort(now);
    core.rings.flush(&mut core.instance, now);
    log_faults_and_alerts(&mut core.instance, &mut core.edges);
}
