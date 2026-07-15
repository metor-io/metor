//! The tuple expansion behind fn-authored systems: [`ExecParamSet`] for
//! parameter tuples and [`ExecuteFn`] for the fns that take them.
//!
//! One macro walk generates the declaration, bind, and per-cycle projections
//! for every arity, so the three can never disagree on parameter order.

use super::param::{BindCx, CycleCx, DeclSink, ExecParam};

/// An execute fn's whole parameter list (past the leading `&mut State`), as
/// one type. Implemented for tuples of [`ExecParam`]s up to arity 16.
pub trait ExecParamSet {
    /// The driver-owned states, one per parameter.
    type State: 'static;
    /// The per-cycle items, one per parameter.
    type Item<'r>;

    fn decls(sink: &mut DeclSink);
    fn bind(cx: &mut BindCx) -> Self::State;
    fn get<'r>(state: &'r mut Self::State, cx: &mut CycleCx<'r>) -> Self::Item<'r>;
    /// Dropped-publish counts summed and cleared across every output state.
    fn take_dropped(state: &mut Self::State) -> u64;
}

/// A fn usable as a system's per-cycle `execute`: a leading `&mut S` state
/// parameter followed by [`ExecParam`]s.
///
/// The `Marker` parameter carries the written parameter types so the blanket
/// impls for different signatures cannot overlap; it is inferred at
/// [`system`](super::system) and never named. The double `FnMut` bound is the
/// standard trick (Bevy's `SystemParamFunction`): the first bound pins each
/// `Pn` from the fn's written signature, the second is the higher-ranked form
/// [`call`](Self::call) invokes with reborrowed items.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a system execute fn",
    note = "an execute fn takes a leading `&mut State` and then only system \
            parameters: `fn(&mut S, now: Timestamp, sensors: &mut Input<A>, \
            out: &mut Output<B>, ...)`. Closures may need their parameter \
            types spelled out."
)]
pub trait ExecuteFn<S, Marker>: 'static {
    /// The parameter tuple past the leading state.
    type Params: ExecParamSet;

    /// Invoke with the state and one cycle's items.
    fn call<'r>(&mut self, state: &'r mut S, params: <Self::Params as ExecParamSet>::Item<'r>);
}

macro_rules! impl_exec {
    ($($p:ident),*) => {
        impl<$($p: ExecParam),*> ExecParamSet for ($($p,)*) {
            type State = ($($p::State,)*);
            type Item<'r> = ($($p::Item<'r>,)*);

            fn decls(_sink: &mut DeclSink) {
                $($p::decl(_sink);)*
            }

            #[allow(clippy::unused_unit)]
            fn bind(_cx: &mut BindCx) -> Self::State {
                ($($p::bind(_cx),)*)
            }

            #[allow(non_snake_case, clippy::unused_unit)]
            fn get<'r>(state: &'r mut Self::State, _cx: &mut CycleCx<'r>) -> Self::Item<'r> {
                let ($($p,)*) = state;
                ($($p::get($p, _cx),)*)
            }

            #[allow(non_snake_case, unused_mut)]
            fn take_dropped(state: &mut Self::State) -> u64 {
                let ($($p,)*) = state;
                let mut total = 0;
                $(total += $p::take_dropped($p);)*
                total
            }
        }

        impl<Func, S, $($p: ExecParam),*> ExecuteFn<S, fn(&mut S, $($p),*)> for Func
        where
            S: 'static,
            Func: 'static
                + FnMut(&mut S, $($p),*)
                + for<'r> FnMut(&'r mut S, $($p::Item<'r>),*),
        {
            type Params = ($($p,)*);

            #[allow(non_snake_case)]
            fn call<'r>(
                &mut self,
                state: &'r mut S,
                params: <Self::Params as ExecParamSet>::Item<'r>,
            ) {
                let ($($p,)*) = params;
                self(state, $($p),*)
            }
        }
    };
}

impl_exec!();
impl_exec!(P1);
impl_exec!(P1, P2);
impl_exec!(P1, P2, P3);
impl_exec!(P1, P2, P3, P4);
impl_exec!(P1, P2, P3, P4, P5);
impl_exec!(P1, P2, P3, P4, P5, P6);
impl_exec!(P1, P2, P3, P4, P5, P6, P7);
impl_exec!(P1, P2, P3, P4, P5, P6, P7, P8);
impl_exec!(P1, P2, P3, P4, P5, P6, P7, P8, P9);
impl_exec!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10);
impl_exec!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11);
impl_exec!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12);
impl_exec!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12, P13);
impl_exec!(P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12, P13, P14);
impl_exec!(
    P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12, P13, P14, P15
);
impl_exec!(
    P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12, P13, P14, P15, P16
);
