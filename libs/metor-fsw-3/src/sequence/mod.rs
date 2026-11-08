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
//!     let mut cmd = seq.on_abort_with(cmd, |seq, cmd| {
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
//! A body registers cleanup with [`Seq::on_abort`], or with
//! [`Seq::on_abort_with`] when the cleanup uses a port the body also writes.
//! If the sequence has been aborted, each handler still registered runs when
//! the body is dropped, innermost first. The handlers also run if the body panics. They
//! do not run when the body fails, times out, or is dropped before it
//! finishes for another reason (a `Reset`, a `Load`, or shutdown).
//!
//! An alarm is a condition the sequence publishes on its `alarm_raised` and
//! `alarm_cleared` outputs. [`alarm!`](macro@crate::alarm) adds one that stays
//! active until the end of the enclosing block, and clears it then. A
//! redline is a critical alarm that also aborts the sequence when it becomes
//! true. [`redline!`](crate::redline) adds one the same way. Each cycle, the
//! step takes the input samples, then steps every active alarm and redline
//! in the order they were added, then polls the body. The first redline
//! whose condition is true logs a `redline` fault with its name and aborts
//! the sequence; dropping the body clears its alarm. A condition may borrow
//! the sequence's parameters, but not the body's locals.
//!
//! A stopped channel does not execute its step at all, so it takes no
//! samples, steps no alarms, and runs no handlers.

mod seq;
mod step;
#[cfg(all(test, feature = "host"))]
mod tests;
mod watch;

use metor_fsw_3_ring::NoWake;

use crate::alarm::{ALARM_CLEARED_PORT, ALARM_RAISED_PORT, AlarmPorts, ClearedMsg, RaisedMsg};
use crate::channel::{ABORT_PORT, AbortRequest};
use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError};
use crate::fn_system::{Defs, FnSignature, InSet, OutSet, Param};
use crate::io::IoPorts;
use crate::port::{Input, Output};
use crate::system::{BindCx, InputBinding, OutputBinding, PortDef, Ports, SystemDef};

pub use seq::{NextCycle, OnAbort, Seq, SeqError, Sleep, WaitFor};
pub(crate) use step::SequenceStep;
pub use watch::{Format, SpecName, Watch, formatter};

/// Adds a redline to `seq` for the rest of the enclosing block.
///
/// A redline is a condition that aborts the sequence when it is true. Each
/// cycle, before the body is polled, the step calls the condition. The first
/// time it returns true, the step raises a critical alarm, logs a `redline`
/// fault with `name` (or the condition's text if no name is given), and
/// aborts the sequence. The abort drops the body, which clears the alarm.
///
/// The alarm's id is `redline.` and the name. The message is the name, or
/// what `fmt` writes.
///
/// The macro pins a [`Watch`] on the stack and links it into `seq`. When the
/// block ends, the `Watch` is dropped and unlinks itself, so its condition is
/// no longer checked.
///
/// There is no `redline!(seq, cond, fmt)`: a format closure takes the named
/// form, `redline!(seq, name, cond, fmt)`.
#[macro_export]
macro_rules! redline {
    ($seq:expr, $cond:expr $(,)?) => {
        $crate::redline!($seq, ::core::stringify!($cond), $cond)
    };
    ($seq:expr, $name:expr, $cond:expr $(,)?) => {
        let __redline = ::core::pin::pin!($crate::Watch::redline(
            $name,
            $cond,
            $crate::sequence::SpecName
        ));
        $seq.watch(__redline.as_ref());
    };
    ($seq:expr, $name:expr, $cond:expr, $fmt:expr $(,)?) => {
        let __redline = ::core::pin::pin!($crate::Watch::redline(
            $name,
            $cond,
            $crate::sequence::formatter($fmt)
        ));
        $seq.watch(__redline.as_ref());
    };
}

/// Adds an alarm to `seq` for the rest of the enclosing block.
///
/// Each cycle, before the body is polled, the step steps the alarm's cell
/// with the condition, a `bool` or an `Option<bool>` whose `None` holds the
/// counts, and publishes a raise or a clear on the sequence's `alarm_raised` and `alarm_cleared` outputs. The
/// message is the spec's id, or what `fmt` writes. When the block ends, an
/// active alarm is cleared.
#[macro_export]
macro_rules! alarm {
    ($seq:expr, $spec:expr, $cond:expr $(,)?) => {
        let __alarm = ::core::pin::pin!($crate::Watch::alarm(
            $spec,
            $cond,
            $crate::sequence::SpecName
        ));
        $seq.watch(__alarm.as_ref());
    };
    ($seq:expr, $spec:expr, $cond:expr, $fmt:expr $(,)?) => {
        let __alarm = ::core::pin::pin!($crate::Watch::alarm(
            $spec,
            $cond,
            $crate::sequence::formatter($fmt)
        ));
        $seq.watch(__alarm.as_ref());
    };
}

/// A sequence written as an async fn.
///
/// `#[sequence]` implements this trait. The fn's parameters are its ports
/// and, optionally, its [`Seq`].
#[allow(async_fn_in_trait)]
pub trait SequenceFn: FnSignature<Wake = NoWake> {
    /// Runs the sequence to its end.
    ///
    /// `state` and `items` share the lifetime `'a`, and the [`Seq`] in
    /// `items` is a `Seq<'a>`. A redline's condition only has to outlive
    /// `'a`, so it may borrow from `state` or from the other items.
    async fn call<'a>(
        state: &'a mut Self::State,
        items: <Self::Params as Param<NoWake>>::Item<'a>,
    ) -> Result<(), SeqError>;
}

/// Returns the port definitions for sequence `S`.
///
/// The inputs are `abort` followed by the parameters' inputs. The outputs
/// are the parameters' outputs, `log`, `alarm_raised`, and `alarm_cleared`.
pub fn def<S: SequenceFn>(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let defs = Defs::of::<S>(cx)?;
    let mut inputs = vec![Input::<AbortRequest>::def(cx, ABORT_PORT)];
    let params_from = inputs.len();
    inputs.extend(defs.inputs);
    let (mut outputs, log) = OutSet::<S>::defs_with_log(cx)?;
    outputs.push(Output::<RaisedMsg>::def(cx, ALARM_RAISED_PORT));
    outputs.push(Output::<ClearedMsg>::def(cx, ALARM_CLEARED_PORT));
    Ok(SystemDef {
        name: S::NAME.into(),
        inputs,
        outputs,
        log: Some(log),
        grants: defs.grants,
        io: defs.io.map(|io| IoPorts {
            cq: params_from + io.cq,
            ..io
        }),
    })
}

/// A sequence's inputs: `abort`, then its parameters' inputs.
pub(crate) struct SeqInputs<S: SequenceFn> {
    pub(crate) abort: Input<AbortRequest>,
    pub(crate) ins: InSet<S>,
}

impl<S: SequenceFn> Ports<InputBinding> for SeqInputs<S> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(def::<S>(cx)?.inputs)
    }

    fn bind(inputs: Vec<InputBinding>, cx: &BindCx) -> Result<Self, MakeError> {
        let mut inputs = inputs.into_iter();
        // PANIC Safety: `def` declares `abort` first, and build binds one
        // list per declared input.
        let abort = Input::new(inputs.next().expect("the abort binding").views);
        Ok(Self {
            abort,
            ins: InSet::bind(inputs.collect(), cx)?,
        })
    }

    fn takes_config_ports() -> bool {
        InSet::<S>::takes_config_ports()
    }
}

/// A sequence's outputs: its params' outputs and `log`, then its alarm ports.
pub(crate) struct SeqOutputs<S: SequenceFn> {
    pub(crate) outs: OutSet<S>,
    pub(crate) alarms: AlarmPorts,
}

impl<S: SequenceFn> Ports<OutputBinding> for SeqOutputs<S> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(def::<S>(cx)?.outputs)
    }

    /// Binds the last two outputs as the alarm ports, and the rest as the
    /// parameters' outputs and `log`.
    fn bind(mut outputs: Vec<OutputBinding>, cx: &BindCx) -> Result<Self, MakeError> {
        // PANIC Safety: `def` declares the two alarm ports last, and build
        // binds one writer per declared output.
        let cleared = outputs.pop().expect("the alarm_cleared binding");
        let raised = outputs.pop().expect("the alarm_raised binding");
        let outs = OutSet::<S>::bind(outputs, cx)?;
        let alarms = AlarmPorts::bind(&cx.instance, raised, cleared, &outs.dropped)?;
        Ok(Self { outs, alarms })
    }

    fn takes_config_ports() -> bool {
        OutSet::<S>::takes_config_ports()
    }
}
