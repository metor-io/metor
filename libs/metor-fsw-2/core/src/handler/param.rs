//! Parameter declarations, binding, and per-cycle borrows for function systems.
//!
//! [`ExecParam`] defines the state a parameter owns and the value lent to the
//! execute function. Tuple implementations walk declarations and bindings in
//! the same order to preserve positional port binding.

use core::any::Any;
use core::sync::atomic::AtomicU64;
use std::sync::Arc;

use metor_fsw_ring::NoWake;
use metor_proto::types::Timestamp;

use crate::binder::AnySource;
use crate::descriptor::PortDesc;
use crate::frame::Frame;
use crate::log::LogPort;
use crate::message::{MsgIn, MsgOut, NamedMsg};
use crate::port::{Input, Output};

use serde::de::DeserializeOwned;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// The declarations one direction-aware walk over an execute fn's parameters
/// produces: the wired ports per direction, in parameter order.
#[derive(Default)]
pub struct DeclSink {
    pub inputs: Vec<PortDesc>,
    pub outputs: Vec<PortDesc>,
    log_params: u8,
    /// The typed-params spec a task fn's `Params<P>` parameter records.
    pub(crate) task_params: Option<super::task::TaskParamsSpec>,
}

impl DeclSink {
    fn count_log(&mut self) {
        self.log_params += 1;
        assert!(
            self.log_params <= 1,
            "an execute fn takes at most one `&mut LogPort` parameter"
        );
    }

    pub(crate) fn set_task_params(&mut self, spec: super::task::TaskParamsSpec) {
        assert!(
            self.task_params.replace(spec).is_none(),
            "a task fn takes at most one `Params<P>` parameter"
        );
    }
}

/// The bind-time context a parameter pulls its ring (and, for a task fn, its
/// decoded params and clock) from.
pub struct BindCx<'a, 'b, 'c> {
    pub(crate) src: &'a mut AnySource<'b, 'c>,
    /// The create-phase decoded `Params<P>` value, taken by its parameter.
    pub(crate) params: Option<Box<dyn Any>>,
    /// A shared dropped-publish cell every by-value output adopts before it
    /// moves into the future, so the driver can report drops on the log.
    pub(crate) drops: Option<Arc<AtomicU64>>,
}

/// The per-cycle context parameters draw non-port items from.
pub struct CycleCx<'r> {
    pub now: Timestamp,
    pub(crate) log: Option<&'r mut LogPort>,
}

/// One parameter of a fn-authored system's execute fn.
///
/// Implemented for `&mut Input<F>`, `&mut MsgIn<M>`, `&mut Output<F>`,
/// `&mut MsgOut<M>`, `Timestamp`, and `&mut LogPort`. Anything else in an
/// execute signature fails the [`ExecuteFn`](super::ExecuteFn) bound.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a system parameter",
    note = "execute fns take `&mut Input<F>`, `&mut MsgIn<M>`, `&mut Output<F>`, \
            `&mut MsgOut<M>`, `now: Timestamp`, or `&mut LogPort` after the \
            leading `&mut State`; other state belongs in the State type"
)]
pub trait ExecParam: Sized {
    /// What the driver owns for this parameter between cycles.
    type State: 'static;
    /// What the fn is lent for one cycle.
    type Item<'r>;

    /// Contribute this parameter's declaration, in parameter order.
    fn decl(sink: &mut DeclSink);
    /// Bind the owned state; ports pull their ring here, in the same order
    /// [`decl`](Self::decl) declared them.
    fn bind(cx: &mut BindCx) -> Self::State;
    /// Lend the per-cycle item out of the owned state.
    fn get<'r>(state: &'r mut Self::State, cx: &mut CycleCx<'r>) -> Self::Item<'r>;
    /// This parameter's dropped-publish count since last asked (outputs only).
    fn take_dropped(_state: &mut Self::State) -> u64 {
        0
    }
}

impl<F> ExecParam for &mut Input<F>
where
    F: Frame + FromBytes + KnownLayout + Immutable + 'static,
{
    type State = Input<F>;
    type Item<'r> = &'r mut Input<F>;

    fn decl(sink: &mut DeclSink) {
        sink.inputs.push(Input::<F>::decl());
    }
    fn bind(cx: &mut BindCx) -> Input<F> {
        Input::bind(cx.src)
    }
    fn get<'r>(state: &'r mut Input<F>, _cx: &mut CycleCx<'r>) -> &'r mut Input<F> {
        state
    }
}

impl<F> ExecParam for &mut Output<F>
where
    F: Frame + IntoBytes + Immutable + 'static,
{
    type State = Output<F>;
    type Item<'r> = &'r mut Output<F>;

    fn decl(sink: &mut DeclSink) {
        sink.outputs.push(Output::<F>::decl());
    }
    fn bind(cx: &mut BindCx) -> Output<F> {
        Output::bind(cx.src)
    }
    fn get<'r>(state: &'r mut Output<F>, _cx: &mut CycleCx<'r>) -> &'r mut Output<F> {
        state
    }
    fn take_dropped(state: &mut Output<F>) -> u64 {
        state.take_dropped()
    }
}

impl<M> ExecParam for &mut MsgIn<M>
where
    M: NamedMsg + DeserializeOwned + 'static,
{
    type State = MsgIn<M>;
    type Item<'r> = &'r mut MsgIn<M>;

    fn decl(sink: &mut DeclSink) {
        sink.inputs.push(MsgIn::<M>::decl());
    }
    fn bind(cx: &mut BindCx) -> MsgIn<M> {
        MsgIn::bind(cx.src)
    }
    fn get<'r>(state: &'r mut MsgIn<M>, _cx: &mut CycleCx<'r>) -> &'r mut MsgIn<M> {
        state
    }
}

impl<M> ExecParam for &mut MsgOut<M>
where
    M: NamedMsg + serde::Serialize + 'static,
{
    type State = MsgOut<M>;
    type Item<'r> = &'r mut MsgOut<M>;

    fn decl(sink: &mut DeclSink) {
        sink.outputs.push(MsgOut::<M>::decl());
    }
    fn bind(cx: &mut BindCx) -> MsgOut<M> {
        MsgOut::bind(cx.src)
    }
    fn get<'r>(state: &'r mut MsgOut<M>, _cx: &mut CycleCx<'r>) -> &'r mut MsgOut<M> {
        state
    }
    fn take_dropped(state: &mut MsgOut<M>) -> u64 {
        state.take_dropped()
    }
}

impl ExecParam for Timestamp {
    type State = ();
    type Item<'r> = Timestamp;

    fn decl(_sink: &mut DeclSink) {}
    fn bind(_cx: &mut BindCx) -> Self::State {}
    fn get<'r>(_state: &'r mut (), cx: &mut CycleCx<'r>) -> Timestamp {
        cx.now
    }
}

impl ExecParam for &mut LogPort<NoWake> {
    type State = ();
    type Item<'r> = &'r mut LogPort;

    fn decl(sink: &mut DeclSink) {
        // The log port is the driver-owned tail, not a wired parameter;
        // only the at-most-one rule is checked here.
        sink.count_log();
    }
    fn bind(_cx: &mut BindCx) -> Self::State {}
    fn get<'r>(_state: &'r mut (), cx: &mut CycleCx<'r>) -> &'r mut LogPort {
        cx.log
            .take()
            .expect("at most one &mut LogPort parameter (checked at decl)")
    }
}
