//! The async authoring form: a system as one `async fn` owning its ports,
//! state in locals.
//!
//! [`Pack::task`](crate::Pack::task) registers an async fn whose parameters
//! are by-value [`TaskParam`]s, moved into the future at bind. A future that
//! returns [`Outcome`] is a sequence; one that returns `()` completed. The driver
//! polls it once per cycle with a no-op waker under the ambient
//! the framework's per-cycle sequence clock, so `wait()`/`now()`/`progress()` work
//! unchanged.

use core::any::Any;
use core::future::Future;
use core::pin::Pin;

use postcard_schema::Schema;
use postcard_schema::schema::NamedType;
use serde::de::DeserializeOwned;

use crate::frame::Frame;
use crate::message::{MsgIn, MsgOut, NamedMsg};
use crate::pack::{EntryParams, MakeError, decode_params};
use crate::port::{Input, Output};
use crate::sequence::Outcome;

use super::param::{BindCx, DeclSink};

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// The typed params of an async-fn system, decoded from the entry's params
/// surface and handed in by value: `async fn f(Params(p): Params<MyParams>, ...)`.
///
/// The trait world cannot express an "any unrecognized parameter type is the
/// params" rule, so the params parameter is named by this wrapper instead (the
/// axum `Json<T>` shape).
pub struct Params<P>(pub P);

/// One by-value parameter of an async-fn system, bound once and moved into
/// the future.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a task parameter",
    note = "async system fns take ports by value (`Input<F>`, `Output<F>`, \
            `MsgIn<M>`, `MsgOut<M>`) or `Params<P>` for typed params"
)]
pub trait TaskParam: Sized + 'static {
    fn decl(sink: &mut DeclSink);
    fn bind(cx: &mut BindCx) -> Self;
}

impl<F> TaskParam for Input<F>
where
    F: Frame + FromBytes + KnownLayout + Immutable + 'static,
{
    fn decl(sink: &mut DeclSink) {
        sink.inputs.push(Input::<F>::decl());
    }
    fn bind(cx: &mut BindCx) -> Self {
        Input::bind(cx.src)
    }
}

impl<F> TaskParam for Output<F>
where
    F: Frame + IntoBytes + Immutable + 'static,
{
    fn decl(sink: &mut DeclSink) {
        sink.outputs.push(Output::<F>::decl());
    }
    fn bind(cx: &mut BindCx) -> Self {
        let mut out: Output<F> = Output::bind(cx.src);
        // Moved into the future, so drops count through the driver's cell.
        if let Some(cell) = &cx.drops {
            out.share_drops(cell.clone());
        }
        out
    }
}

impl<M> TaskParam for MsgIn<M>
where
    M: NamedMsg + DeserializeOwned + 'static,
{
    fn decl(sink: &mut DeclSink) {
        sink.inputs.push(MsgIn::<M>::decl());
    }
    fn bind(cx: &mut BindCx) -> Self {
        MsgIn::bind(cx.src)
    }
}

impl<M> TaskParam for MsgOut<M>
where
    M: NamedMsg + serde::Serialize + 'static,
{
    fn decl(sink: &mut DeclSink) {
        sink.outputs.push(MsgOut::<M>::decl());
    }
    fn bind(cx: &mut BindCx) -> Self {
        let mut out: MsgOut<M> = MsgOut::bind(cx.src);
        if let Some(cell) = &cx.drops {
            out.share_drops(cell.clone());
        }
        out
    }
}

impl<P> TaskParam for Params<P>
where
    P: DeserializeOwned + Schema + 'static,
{
    fn decl(sink: &mut DeclSink) {
        sink.set_task_params(TaskParamsSpec::of::<P>());
    }
    fn bind(cx: &mut BindCx) -> Self {
        let any = cx
            .params
            .take()
            .expect("task params decoded at create (declared by the decl walk)");
        Params(
            *any.downcast::<P>()
                .expect("params decode type matches the declared Params<P>"),
        )
    }
}

/// How an async-fn entry's params are decoded, recorded by the decl walk so
/// the create phase can decode without knowing `P`.
pub(crate) struct TaskParamsSpec {
    pub(crate) schema: &'static NamedType,
    pub(crate) decode: fn(EntryParams<'_>) -> Result<Box<dyn Any>, MakeError>,
}

impl TaskParamsSpec {
    fn of<P: DeserializeOwned + Schema + 'static>() -> Self {
        Self {
            schema: P::SCHEMA,
            decode: |ep| Ok(Box::new(decode_params::<P>(ep)?) as Box<dyn Any>),
        }
    }

    /// The paramless default: decode `()` (rejecting stray config).
    pub(crate) fn unit() -> Self {
        Self::of::<()>()
    }
}

/// What an async system fn's future may resolve to.
pub trait IntoOutcome {
    fn into_outcome(self) -> Outcome;
}

impl IntoOutcome for Outcome {
    fn into_outcome(self) -> Outcome {
        self
    }
}

/// A plain async system that returns simply completed.
impl IntoOutcome for () {
    fn into_outcome(self) -> Outcome {
        Outcome::Completed
    }
}

/// An async fn usable as a system: by-value [`TaskParam`]s, returning a
/// future whose output is an [`Outcome`] (a sequence) or `()` (a task).
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an async system fn",
    note = "a task fn takes ports by value and returns a future: \
            `async fn f(mut cmds: MsgIn<Cmd>, mut tm: Output<Tm>) -> Outcome` \
            (or no return for a free-running task)"
)]
pub trait AsyncSystemFn<Marker>: 'static {
    /// Contribute every parameter's declaration, in signature order.
    fn decls(sink: &mut DeclSink);
    /// Bind the parameters and build the future, moving them in.
    fn build(self, cx: &mut BindCx) -> Pin<Box<dyn Future<Output = Outcome>>>;
}

macro_rules! impl_task {
    ($($p:ident),*) => {
        impl<Func, Fut, $($p: TaskParam),*> AsyncSystemFn<fn($($p),*) -> Fut> for Func
        where
            Func: 'static + FnOnce($($p),*) -> Fut,
            Fut: Future + 'static,
            Fut::Output: IntoOutcome,
        {
            fn decls(_sink: &mut DeclSink) {
                $($p::decl(_sink);)*
            }

            #[allow(non_snake_case)]
            fn build(self, _cx: &mut BindCx) -> Pin<Box<dyn Future<Output = Outcome>>> {
                $(let $p = $p::bind(_cx);)*
                Box::pin(async move { self($($p),*).await.into_outcome() })
            }
        }
    };
}

impl_task!();
impl_task!(P1);
impl_task!(P1, P2);
impl_task!(P1, P2, P3);
impl_task!(P1, P2, P3, P4);
impl_task!(P1, P2, P3, P4, P5);
impl_task!(P1, P2, P3, P4, P5, P6);
impl_task!(P1, P2, P3, P4, P5, P6, P7);
impl_task!(P1, P2, P3, P4, P5, P6, P7, P8);
impl_task!(P1, P2, P3, P4, P5, P6, P7, P8, P9);
impl_task!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10);
impl_task!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11);
impl_task!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12);
