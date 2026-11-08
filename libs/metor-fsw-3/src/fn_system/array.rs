//! `[P; N]` as a system parameter.

use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError};
use crate::frame::Frame;
use crate::port::{Input, InputWake, Output, Sample};
use crate::record::Record;

use super::param::{Cycle, Defs, Param, Refresh, Views, Writers};
use super::path::ParamPath;

/// A parameter that can be an array element.
///
/// Element `i` of `[P; N]` is named `{path}.{i}`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be an array element of a system parameter",
    note = "arrays hold `Input`, `Sample`, `Output`, arrays of those, and `SystemParams` structs"
)]
pub trait ArrayParam {}

impl<T: Record + ?Sized, W: InputWake> ArrayParam for Input<T, W> {}
impl<T: Record + ?Sized> ArrayParam for Output<T> {}
impl<T: Frame + Copy> ArrayParam for Sample<T> {}
impl<P: ArrayParam, const N: usize> ArrayParam for [P; N] {}

impl<W: InputWake, P: Param<W> + ArrayParam, const N: usize> Param<W> for [P; N] {
    type In = [P::In; N];
    type Out = [P::Out; N];
    type Item<'a>
        = [P::Item<'a>; N]
    where
        Self: 'a;

    fn append_defs(at: &ParamPath<'_>, cx: &DefCx<'_>, defs: &mut Defs) -> Result<(), DefError> {
        (0..N).try_for_each(|i| P::append_defs(&at.index(i), cx, defs))
    }

    fn bind_in(views: &mut Views<W>) -> Self::In {
        core::array::from_fn(|_| P::bind_in(views))
    }

    fn bind_out(at: &ParamPath<'_>, writers: &mut Writers) -> Result<Self::Out, MakeError> {
        let outs = (0..N)
            .map(|i| P::bind_out(&at.index(i), writers))
            .collect::<Result<Vec<_>, _>>()?;
        // PANIC Safety: `outs` holds one output per index in `0..N`.
        Ok(outs.try_into().ok().expect("one output per element"))
    }

    fn get<'a>(
        input: &'a mut Self::In,
        output: &'a mut Self::Out,
        cx: &mut Cycle<'a>,
    ) -> Self::Item<'a> {
        let mut pairs = input.iter_mut().zip(output.iter_mut());
        core::array::from_fn(|_| {
            // PANIC Safety: both arrays hold `N` elements.
            let (input, output) = pairs.next().expect("one pair per element");
            P::get(input, output, cx)
        })
    }

    fn refresh(input: &mut Self::In) {
        input.iter_mut().for_each(P::refresh);
    }

    fn get_sampled<'a>(
        input: &'a mut Self::In,
        output: &'a mut Self::Out,
        cx: &mut Cycle<'a>,
    ) -> (Self::Item<'a>, impl Refresh + use<'a, W, P, N>) {
        let mut refreshes = Vec::with_capacity(N);
        let mut pairs = input.iter_mut().zip(output.iter_mut());
        let items = core::array::from_fn(|_| {
            // PANIC Safety: both arrays hold `N` elements.
            let (input, output) = pairs.next().expect("one pair per element");
            let (item, refresh) = P::get_sampled(input, output, cx);
            refreshes.push(refresh);
            item
        });
        (items, refreshes)
    }
}

impl<R: Refresh> Refresh for Vec<R> {
    fn refresh(&mut self) {
        self.iter_mut().for_each(R::refresh);
    }
}
