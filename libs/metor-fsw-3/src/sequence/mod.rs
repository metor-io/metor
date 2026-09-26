//! Sequences: async fns the coordinator polls once per cycle.
//!
//! A `#[sequence]` fn takes its ports as a `#[system]` fn does, plus a
//! [`Seq`] handle for the cycle's time, its waits, and whether it was
//! aborted. Its step owns the future and polls it with a waker that does
//! nothing: a wait resolves on the first poll that satisfies it, never on a
//! wake.
//!
//! ```
//! # use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//! # use metor_fsw_3::{Frame, Timestamp};
//! # #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! # #[repr(C)]
//! # struct Arm {
//! #     #[frame(timestamp)]
//! #     timestamp: Timestamp,
//! #     on: u64,
//! # }
//! use core::time::Duration;
//! use metor_fsw_3::{Input, Output, Seq, SeqError, SystemTable, sequence};
//!
//! /// Arms once the other side reports armed, then holds for half a second.
//! #[sequence]
//! async fn arm(seq: &Seq, report: &mut Input<Arm>, cmd: &mut Output<Arm>) -> Result<(), SeqError> {
//!     let _ = cmd.write(&Arm { timestamp: seq.now(), on: 1 });
//!     let armed = || report.latest().ok().flatten().is_some_and(|r| r.on == 1);
//!     seq.wait_for(Duration::from_secs(1), armed).await?;
//!     seq.sleep(Duration::from_millis(500)).await
//! }
//!
//! let mut table = SystemTable::new();
//! table.register_sequence("arm", arm(), Default::default)?;
//! # Ok::<(), metor_fsw_3::BuildError>(())
//! ```
//!
//! Every sequence reads an `abort` input first among its ports. An abort
//! record, or a body calling [`Seq::abort`], raises the abort flag, which
//! never comes down. At the next cycle's start the step drops the body
//! without polling it and ends [`SeqError::Aborted`]. Dropping the body runs
//! each armed [`Seq::on_abort`] handler, innermost first. A panic in the body
//! runs them too. A failure, a timeout, or a body dropped unfinished
//! (`Reset`, `Load`, shutdown) runs none.

mod seq;
mod step;
#[cfg(test)]
mod tests;

use std::borrow::Cow;

use metor_fsw_3_ring::NoWake;

use crate::channel::{ABORT_PORT, AbortRequest};
use crate::coordinator::{TableEntry, bindings, refuse_thread};
use crate::def::{DefCx, DefError};
use crate::fn_system::{Ctor, InSet, OutSet, Param, Ports};
use crate::port::Input;
use crate::system::{SystemDef, SystemInputs, SystemOutputs};

pub use seq::{NextCycle, OnAbort, Seq, SeqError, Sleep, WaitFor};
pub(crate) use step::SequenceStep;

/// A `SequenceFn` is a sequence whose parameters are its ports and its [`Seq`].
#[allow(async_fn_in_trait)]
pub trait SequenceFn: Ports {
    /// Runs the sequence to its end.
    async fn call(
        state: &mut Self::State,
        items: <Self::Params as Param>::Item<'_, NoWake>,
    ) -> Result<(), SeqError>;
}

/// A sequence's ports: `abort`, then its parameters' inputs; its
/// parameters' outputs, then `log`.
pub fn def<S: SequenceFn>(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let mut inputs = vec![Input::<AbortRequest>::def(cx, ABORT_PORT)];
    inputs.extend(InSet::<S>::defs(cx)?);
    Ok(SystemDef {
        name: S::NAME.into(),
        inputs,
        outputs: OutSet::<S>::defs(cx)?,
    })
}

/// The table entry a `#[sequence]` fn registers as.
pub(crate) fn entry<S: SequenceFn, M, C: Ctor<S::State, M> + 'static>(
    ctor: C,
) -> Result<TableEntry, DefError> {
    Ok(TableEntry {
        def: def::<S>(&DefCx::empty())?,
        def_fn: Box::new(def::<S>),
        doc: Cow::Borrowed(S::DOC),
        schema: C::schema(),
        make: std::sync::Arc::new(move |cx| {
            refuse_thread(cx.adapter)?;
            let state = ctor.make(cx.params)?;
            let (views, writers) = bindings(cx.def, cx.inputs, cx.outputs);
            Ok(Box::new(SequenceStep::new::<S>(state, views, writers)))
        }),
    })
}
