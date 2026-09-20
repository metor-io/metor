//! The [`Param`] trait, one per `execute` parameter type, and its tuple impls.

use std::borrow::Cow;

use metor_fsw_3_ring::WakeSink;
use metor_proto::types::Timestamp;

use crate::def::{DefCx, DefError};
use crate::log::Log;
use crate::port::{DynInputs, DynOutputs, Input, Output};
use crate::record::Record;
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

/// The bindings a bundle binds from, in parameter order: each parameter takes
/// the share its [`Defs`] walk appended.
pub struct Bindings<B> {
    items: std::vec::IntoIter<B>,
    counts: std::vec::IntoIter<usize>,
}

impl<B> Bindings<B> {
    /// Hands out `items` in the shares `counts` names.
    pub(crate) fn new(items: Vec<B>, counts: Vec<usize>) -> Self {
        Self {
            items: items.into_iter(),
            counts: counts.into_iter(),
        }
    }

    /// The one binding the next parameter takes.
    pub(crate) fn next(&mut self) -> Option<B> {
        self.counts.next()?;
        self.items.next()
    }

    /// Every binding the next parameter takes.
    pub(crate) fn take(&mut self) -> Vec<B> {
        let count = self.counts.next().unwrap_or_default();
        self.items.by_ref().take(count).collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.len() == 0
    }
}

/// "Views" are bound inputs
pub type Views<W> = Bindings<InputBinding<W>>;
/// "Writers" are bound outputs
pub type Writers = Bindings<OutputBinding>;
/// A bundles parameter names
pub type ParamNames<'n> = core::slice::Iter<'n, &'static str>;

/// The ports a system's parameters take under one instance's config, and how
/// many each of them took.
#[derive(Default)]
pub struct Defs {
    pub inputs: Vec<PortDef>,
    pub outputs: Vec<PortDef>,
    /// One count per input parameter, in parameter order.
    pub(crate) input_counts: Vec<usize>,
    /// One count per output parameter, in parameter order.
    pub(crate) output_counts: Vec<usize>,
    declared_inputs: Vec<Cow<'static, str>>,
    declared_outputs: Vec<Cow<'static, str>>,
}

impl Defs {
    /// Every port a system's parameters take under `cx`, under the names it gave them.
    pub fn of<S: crate::fn_system::Ports>(cx: &DefCx<'_>) -> Result<Self, DefError> {
        let declared = Self::walk::<S>(&DefCx::empty(), Self::default())?;
        Self::walk::<S>(
            cx,
            Self {
                declared_inputs: names(&declared.inputs),
                declared_outputs: names(&declared.outputs),
                ..Self::default()
            },
        )
    }

    fn walk<S: crate::fn_system::Ports>(cx: &DefCx<'_>, mut defs: Self) -> Result<Self, DefError> {
        S::Params::append_defs(&mut S::NAMES.iter(), cx, &mut defs)?;
        Ok(defs)
    }

    /// Appends the one port a declared parameter names.
    pub fn push_input(&mut self, def: PortDef) {
        self.inputs.push(def);
        self.input_counts.push(1);
    }

    pub fn push_output(&mut self, def: PortDef) {
        self.outputs.push(def);
        self.output_counts.push(1);
    }

    /// Appends the ports one parameter takes from the config.
    pub fn push_inputs(&mut self, defs: Vec<PortDef>) {
        self.input_counts.push(defs.len());
        self.inputs.extend(defs);
    }

    pub fn push_outputs(&mut self, defs: Vec<PortDef>) {
        self.output_counts.push(defs.len());
        self.outputs.extend(defs);
    }

    /// The input ports the type declares whatever its config.
    pub fn declared_inputs(&self) -> &[Cow<'static, str>] {
        &self.declared_inputs
    }

    /// The output ports the type declares whatever its config.
    pub fn declared_outputs(&self) -> &[Cow<'static, str>] {
        &self.declared_outputs
    }
}

fn names(ports: &[PortDef]) -> Vec<Cow<'static, str>> {
    ports.iter().map(|port| port.name.clone()).collect()
}

/// `Cycle` is the extra parameter passed to execute beyonds its ports.
pub struct Cycle<'a> {
    pub now: Timestamp,
    log: Option<&'a mut Log>,
}

impl<'a> Cycle<'a> {
    pub(crate) fn new(now: Timestamp, log: &'a mut Log) -> Self {
        Self {
            now,
            log: Some(log),
        }
    }

    /// Hands the log to the one `&mut Log` parameter.
    pub fn take_log(&mut self) -> &'a mut Log {
        // PANIC Safety: only a system with two `&mut Log` parameters reaches
        // this, on its first cycle; the message names the fix.
        self.log
            .take()
            .expect("a system takes `&mut Log` at most once")
    }
}

/// Param is implemented for parameter types for system fns, and for tuples of them.
///
/// `W` is the wake an input carries: `NoWake` on the cycle thread, `Notifier`
/// on a background thread. Outputs never wake anyone, since the cycle thread
/// drains an async system's outputs itself.
///
/// For instance [`Input`] implements `Param` to bind an input port.
/// The goal is to let system fns define what parameters they need from the type system.
pub trait Param {
    /// The bound input, or `()`.
    type In<W: WakeSink + Clone + 'static>;
    /// The bound writer, or `()`.
    type Out;
    /// The value `execute` receives.
    type Item<'a, W: WakeSink + Clone + 'static>
    where
        Self: 'a;

    /// Appends the ports this parameter takes under this config, if it takes
    /// any, naming them from `names`.
    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError>;

    /// Returns this parameter's input (if one exists) by popping a view from `views`.
    fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W>;

    /// Returns this parameter's output (if one exists) by popping a writer from `writers`.
    fn bind_out(writers: &mut Writers) -> Self::Out;

    /// Returns the value that will be passed into `execute`
    fn get<'a, W: WakeSink + Clone + 'static>(
        input: &'a mut Self::In<W>,
        output: &'a mut Self::Out,
        cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W>;
}

/// Takes the next leaf parameter's name.
fn name(names: &mut ParamNames<'_>) -> &'static str {
    // PANIC Safety: `#[system]` emits one name per parameter.
    names.next().expect("one name per parameter")
}

/// Both `Input<T>` and `Input<T, Notifier>` name the same port; the wiring
/// picks which one a system is handed.
impl<T: Record + 'static + ?Sized, D: WakeSink + 'static> Param for Input<T, D> {
    type In<W: WakeSink + Clone + 'static> = Input<T, W>;
    type Out = ();
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut Input<T, W>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.push_input(Input::<T>::def(name(names)));
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W> {
        // PANIC Safety: the coordinator binds one list per declared input and
        // validates alignment at build; a mismatch is a coordinator bug.
        let binding = views.next().expect("one binding per input param");
        Input::try_new(binding.views).expect("alignment checked at build")
    }

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a, W: WakeSink + Clone + 'static>(
        input: &'a mut Self::In<W>,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W> {
        input
    }
}

impl<T: Record + 'static + ?Sized> Param for Output<T> {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = Output<T>;
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut Output<T>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.push_output(Output::<T>::def(name(names)));
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        // PANIC Safety: the coordinator binds one writer per declared output and
        // validates alignment at build; a mismatch is a coordinator bug.
        let binding = writers.next().expect("one binding per output param");
        Output::try_new(binding.writer).expect("alignment checked at build")
    }

    fn get<'a, W: WakeSink + Clone + 'static>(
        _input: &'a mut (),
        output: &'a mut Self::Out,
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W> {
        output
    }
}

impl<D: WakeSink + 'static> Param for DynInputs<D> {
    type In<W: WakeSink + Clone + 'static> = DynInputs<W>;
    type Out = ();
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut DynInputs<W>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        name(names);
        let ports = crate::port::dyn_input_defs(cx, defs.declared_inputs())?;
        defs.push_inputs(ports);
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W> {
        SystemInputs::bind(views.take())
    }

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a, W: WakeSink + Clone + 'static>(
        input: &'a mut Self::In<W>,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W> {
        input
    }
}

impl Param for DynOutputs {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = DynOutputs;
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut DynOutputs;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        name(names);
        let ports = crate::port::dyn_output_defs(cx, defs.declared_outputs())?;
        defs.push_outputs(ports);
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        SystemOutputs::bind(writers.take())
    }

    fn get<'a, W: WakeSink + Clone + 'static>(
        _input: &'a mut (),
        output: &'a mut Self::Out,
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W> {
        output
    }
}

impl Param for Timestamp {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = ();
    type Item<'a, W: WakeSink + Clone + 'static> = Timestamp;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        _defs: &mut Defs,
    ) -> Result<(), DefError> {
        name(names);
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a, W: WakeSink + Clone + 'static>(
        _input: &'a mut (),
        _output: &'a mut (),
        cx: &mut Cycle<'a>,
    ) -> Timestamp {
        cx.now
    }
}

impl Param for Log {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = ();
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut Log;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        _defs: &mut Defs,
    ) -> Result<(), DefError> {
        name(names);
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a, W: WakeSink + Clone + 'static>(
        _input: &'a mut (),
        _output: &'a mut (),
        cx: &mut Cycle<'a>,
    ) -> &'a mut Log {
        cx.take_log()
    }
}

/// A tuple of `Param`s is a `Param`: each half is the tuple of the elements' halves.
macro_rules! impl_param_for_tuple {
    ($(($P:ident, $i:tt)),*) => {
        impl<$($P: Param),*> Param for ($($P,)*) {
            type In<W: WakeSink + Clone + 'static> = ($($P::In<W>,)*);
            type Out = ($($P::Out,)*);
            type Item<'a, W: WakeSink + Clone + 'static> = ($($P::Item<'a, W>,)*) where Self: 'a;

            #[allow(unused_variables)]
            fn append_defs(names: &mut ParamNames<'_>, cx: &DefCx<'_>, defs: &mut Defs) -> Result<(), DefError> {
                $( $P::append_defs(names, cx, defs)?; )*
                Ok(())
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W> {
                ($( $P::bind_in::<W>(views), )*)
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_out(writers: &mut Writers) -> Self::Out {
                ($( $P::bind_out(writers), )*)
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn get<'a, W: WakeSink + Clone + 'static>(input: &'a mut Self::In<W>, output: &'a mut Self::Out, cx: &mut Cycle<'a>) -> Self::Item<'a, W> {
                ($( $P::get::<W>(&mut input.$i, &mut output.$i, cx), )*)
            }
        }
    };
}

impl_param_for_tuple!();
impl_param_for_tuple!((A, 0));
impl_param_for_tuple!((A, 0), (B, 1));
impl_param_for_tuple!((A, 0), (B, 1), (C, 2));
impl_param_for_tuple!((A, 0), (B, 1), (C, 2), (D, 3));
impl_param_for_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4));
impl_param_for_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5));
impl_param_for_tuple!((A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6));
impl_param_for_tuple!(
    (A, 0),
    (B, 1),
    (C, 2),
    (D, 3),
    (E, 4),
    (F, 5),
    (G, 6),
    (H, 7)
);
impl_param_for_tuple!(
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
impl_param_for_tuple!(
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
impl_param_for_tuple!(
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
impl_param_for_tuple!(
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
impl_param_for_tuple!(
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
impl_param_for_tuple!(
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
impl_param_for_tuple!(
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
impl_param_for_tuple!(
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
