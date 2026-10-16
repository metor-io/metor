//! Sequences: async fns that run across many cycles.
//!
//! The coordinator polls each sequence once per cycle until it finishes. A
//! `#[sequence]` fn takes its ports as parameters, just like a `#[system]`
//! fn. It can also take a [`Seq`] handle, which tells the body the current
//! cycle time and whether it has been aborted, and lets it wait. The
//! sequence's step owns the body's future and polls it with a waker that
//! does nothing. Nothing ever wakes a sequence: a wait resolves on the first
//! poll where its condition is met.
//!
//! ```
//! # use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//! # use metor_fsw_3::{Frame, Timestamp};
//! # #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
//! # #[repr(C)]
//! # struct Arm {
//! #     #[frame(timestamp)]
//! #     timestamp: Timestamp,
//! #     on: u64,
//! # }
//! # #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
//! # #[repr(C)]
//! # struct Bus {
//! #     #[frame(timestamp)]
//! #     timestamp: Timestamp,
//! #     volts: f64,
//! # }
//! use core::time::Duration;
//! use metor_fsw_3::{Input, Output, Sample, Seq, SeqError, SystemTable, redline, sequence};
//!
//! /// Arms once the other side reports armed, then holds for half a second.
//! /// Disarms if aborted, and aborts if the bus sags.
//! #[sequence]
//! async fn arm(
//!     seq: &Seq,
//!     report: &mut Input<Arm>,
//!     bus: &Sample<Bus>,
//!     cmd: &mut Output<Arm>,
//! ) -> Result<(), SeqError> {
//!     let _disarm = seq.on_abort(|seq| {
//!         let _ = cmd.write(&Arm { timestamp: seq.now(), on: 0 });
//!     });
//!     redline!(seq, "bus voltage", || bus.get().is_some_and(|b| b.volts < 24.0));
//!     let _ = cmd.write(&Arm { timestamp: seq.now(), on: 1 });
//!     let armed = || report.latest().ok().flatten().is_some_and(|r| r.on == 1);
//!     seq.wait_for(Duration::from_secs(1), armed)
//!         .await
//!         .map_err(|_| seq.abort())?;
//!     seq.sleep(Duration::from_millis(500)).await
//! }
//!
//! let mut table = SystemTable::new();
//! table.register_sequence("arm", arm(), Default::default)?;
//! # Ok::<(), metor_fsw_3::BuildError>(())
//! ```
//!
//! Every sequence has an `abort` input, which comes before its other
//! inputs. The sequence is aborted when a record arrives on that input or
//! when the body calls [`Seq::abort`]. Once aborted, it stays aborted. At
//! the start of the next cycle the step drops the body without polling it,
//! and the sequence ends with [`SeqError::Aborted`].
//!
//! A body registers cleanup with [`Seq::on_abort`]. If the sequence has
//! been aborted, each handler still registered runs when the body is
//! dropped, innermost first. The handlers also run if the body panics. They
//! do not run when the body fails, times out, or is dropped before it
//! finishes for another reason (a `Reset`, a `Load`, or shutdown).
//!
//! A redline is a condition that aborts the sequence when it becomes true.
//! [`redline!`](crate::redline) adds one that stays active until the end of
//! the enclosing block. Each cycle, the step takes the input samples, then
//! checks the active redlines in the order they were added, then polls the
//! body. The first redline whose condition is true logs a `redline` fault
//! with its name and aborts the sequence. A condition may borrow the
//! sequence's parameters, but not the body's locals.
//!
//! A stopped channel does not execute its step at all, so it takes no
//! samples, checks no redlines, and runs no handlers.

mod redline;
mod seq;
mod step;
#[cfg(all(test, feature = "host"))]
mod tests;

use std::borrow::Cow;

use metor_fsw_3_ring::NoWake;

use crate::channel::{ABORT_PORT, AbortRequest};
use crate::coordinator::{SystemKind, TableEntry, bindings, refuse_thread};
use crate::def::{DefCx, DefError};
use crate::fn_system::{Ctor, Defs, FnSignature, InSet, OutSet, Param};
use crate::port::Input;
use crate::system::{Ports, SystemDef};

pub use redline::Redline;
pub use seq::{NextCycle, OnAbort, Seq, SeqError, Sleep, WaitFor};
pub(crate) use step::SequenceStep;

/// Adds a redline to `seq` for the rest of the enclosing block.
///
/// A redline is a condition that aborts the sequence when it is true. Each
/// cycle, before the body is polled, the step calls the condition. The first
/// time it returns true, the step logs a `redline` fault with `name` (or
/// `"redline"` if no name is given) and aborts the sequence.
///
/// The macro pins a [`Redline`] on the stack and links it
/// into `seq`. When the block ends, the `Redline` is dropped and unlinks
/// itself, so its condition is no longer checked.
#[macro_export]
macro_rules! redline {
    ($seq:expr, $cond:expr $(,)?) => {
        $crate::redline!($seq, "redline", $cond)
    };
    ($seq:expr, $name:expr, $cond:expr $(,)?) => {
        let __redline = ::core::pin::pin!($crate::Redline::new($name, $cond));
        $seq.redline(__redline.as_ref());
    };
}

/// A sequence written as an async fn.
///
/// `#[sequence]` implements this trait. The fn's parameters are its ports
/// and, optionally, its [`Seq`].
#[allow(async_fn_in_trait)]
pub trait SequenceFn: FnSignature {
    /// Runs the sequence to its end.
    ///
    /// `state` and `items` share the lifetime `'a`, and the [`Seq`] in
    /// `items` is a `Seq<'a>`. A redline's condition only has to outlive
    /// `'a`, so it may borrow from `state` or from the other items.
    async fn call<'a>(
        state: &'a mut Self::State,
        items: <Self::Params as Param>::Item<'a, NoWake>,
    ) -> Result<(), SeqError>;
}

/// Returns the port definitions for sequence `S`.
///
/// The inputs are `abort` followed by the parameters' inputs. The outputs
/// are the parameters' outputs followed by `log`.
pub fn def<S: SequenceFn>(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let mut inputs = vec![Input::<AbortRequest>::def(cx, ABORT_PORT)];
    inputs.extend(InSet::<S>::defs(cx)?);
    Ok(SystemDef {
        name: S::NAME.into(),
        inputs,
        outputs: OutSet::<S>::defs(cx)?,
        grants: Defs::of::<S>(cx)?.grants,
    })
}

/// Builds the system table entry for a `#[sequence]` fn.
///
/// `ctor` creates the sequence's state each time the sequence is made.
pub(crate) fn entry<S: SequenceFn, M, C: Ctor<S::State, M> + 'static>(
    ctor: C,
) -> Result<TableEntry, DefError> {
    Ok(TableEntry {
        kind: SystemKind::Sequence,
        def: def::<S>(&DefCx::empty())?,
        def_fn: Box::new(def::<S>),
        doc: Cow::Borrowed(S::DOC),
        schema: C::schema(),
        make: std::sync::Arc::new(move |cx| {
            refuse_thread(cx.adapter)?;
            let instance = crate::fn_system::Instance::from_make_cx(&cx);
            let (views, writers) = bindings(cx.def, cx.inputs, cx.outputs);
            let mut views = views.into_iter();
            // PANIC Safety: `def` declares `abort` first, and build binds one
            // list per declared input.
            let abort = Input::new(views.next().expect("the abort binding").views);
            let (ins, outs, io) = crate::fn_system::bind::<S>(views.collect(), writers, instance)?;
            let state = crate::fn_system::make_state(&ctor, cx.id, cx.params, io.as_ref())?;
            Ok(Box::new(SequenceStep::new::<S>(state, abort, ins, outs)))
        }),
    })
}
