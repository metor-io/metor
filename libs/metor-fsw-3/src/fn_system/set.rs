//! [`ParamSet`] over tuples of [`Param`], and the bundles that bind one.

use metor_fsw_3_ring::{NoWake, View, Writer};
use metor_proto::types::Timestamp;

use crate::system::{PortDef, SystemInputs, SystemOutputs};

use super::SystemFn;
use super::param::{Param, Views, Writers};

/// A `ParamSet` is the tuple of an `execute` method's parameters, bound and handed over together.
pub trait ParamSet {
    type Ins;
    type Outs;
    type Items<'a>
    where
        Self: 'a;

    /// Declares every port, naming each by its entry in `names`.
    fn defs(names: &[&'static str], inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>);

    /// Binds every input from `views`, in parameter order.
    fn bind_ins(views: &mut Views) -> Self::Ins;

    /// Binds every output from `writers`, in parameter order.
    fn bind_outs(writers: &mut Writers) -> Self::Outs;

    /// Produces every parameter's value for one cycle.
    fn get<'a>(ins: &'a mut Self::Ins, outs: &'a mut Self::Outs, now: Timestamp)
    -> Self::Items<'a>;
}

macro_rules! impl_param_set {
    ($(($P:ident, $i:tt)),*) => {
        impl<$($P: Param),*> ParamSet for ($($P,)*) {
            type Ins = ($($P::In,)*);
            type Outs = ($($P::Out,)*);
            type Items<'a> = ($($P::Item<'a>,)*) where Self: 'a;

            #[allow(unused_variables)]
            fn defs(names: &[&'static str], inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>) {
                // PANIC Safety: `#[system]` emits one name per parameter.
                $( $P::defs(names[$i], inputs, outputs); )*
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_ins(views: &mut Views) -> Self::Ins {
                ($( $P::bind_in(views), )*)
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_outs(writers: &mut Writers) -> Self::Outs {
                ($( $P::bind_out(writers), )*)
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn get<'a>(ins: &'a mut Self::Ins, outs: &'a mut Self::Outs, now: Timestamp) -> Self::Items<'a> {
                ($( $P::get(&mut ins.$i, &mut outs.$i, now), )*)
            }
        }
    };
}

impl_param_set!();
impl_param_set!((A, 0));
impl_param_set!((A, 0), (B, 1));
impl_param_set!((A, 0), (B, 1), (C, 2));
impl_param_set!((A, 0), (B, 1), (C, 2), (D, 3));
impl_param_set!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4));
impl_param_set!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5));
impl_param_set!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6));
impl_param_set!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7)
);
impl_param_set!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7),
    (I, 8)
);
impl_param_set!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7),
    (I, 8),
    (J, 9)
);
impl_param_set!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7),
    (I, 8),
    (J, 9),
    (K, 10)
);
impl_param_set!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7),
    (I, 8),
    (J, 9),
    (K, 10),
    (L, 11)
);
impl_param_set!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7),
    (I, 8),
    (J, 9),
    (K, 10),
    (L, 11),
    (M, 12)
);
impl_param_set!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7),
    (I, 8),
    (J, 9),
    (K, 10),
    (L, 11),
    (M, 12),
    (N, 13)
);
impl_param_set!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7),
    (I, 8),
    (J, 9),
    (K, 10),
    (L, 11),
    (M, 12),
    (N, 13),
    (O, 14)
);
impl_param_set!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7),
    (I, 8),
    (J, 9),
    (K, 10),
    (L, 11),
    (M, 12),
    (N, 13),
    (O, 14),
    (P, 15)
);

/// An `InSet` is the bound inputs of a fn system's parameters.
pub struct InSet<S: SystemFn>(pub(crate) <S::Params as ParamSet>::Ins);

/// An `OutSet` is the bound outputs of a fn system's parameters.
pub struct OutSet<S: SystemFn>(pub(crate) <S::Params as ParamSet>::Outs);

impl<S: SystemFn> SystemInputs for InSet<S> {
    fn defs() -> Vec<PortDef> {
        let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
        S::Params::defs(S::NAMES, &mut inputs, &mut outputs);
        inputs
    }

    fn bind(views: Vec<Vec<View<NoWake>>>) -> Self {
        let mut views = views.into_iter();
        let ins = S::Params::bind_ins(&mut views);
        // PANIC Safety: the coordinator binds exactly the declared ports.
        assert!(views.next().is_none(), "more view lists than input params");
        Self(ins)
    }
}

impl<S: SystemFn> SystemOutputs for OutSet<S> {
    fn defs() -> Vec<PortDef> {
        let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
        S::Params::defs(S::NAMES, &mut inputs, &mut outputs);
        outputs
    }

    fn bind(writers: Vec<Writer<NoWake>>) -> Self {
        let mut writers = writers.into_iter();
        let outs = S::Params::bind_outs(&mut writers);
        // PANIC Safety: the coordinator binds exactly the declared ports.
        assert!(writers.next().is_none(), "more writers than output params");
        Self(outs)
    }
}
