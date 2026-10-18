//! The [`Param`] trait, which defines what a system fn can take as a
//! parameter.

use core::sync::atomic::AtomicU32;
use std::borrow::Cow;
use std::sync::Arc;

use metor_fsw_3_ring::WakeSink;
use metor_proto::types::Timestamp;

use crate::{
    Rng,
    def::{DefCx, DefError, Records},
    frame::Frame,
    log::Log,
    port::{DynInputs, DynOutputs, Input, Output, Sample, Samplers},
    record::Record,
    system::{InputBinding, OutputBinding, PortDef, Ports},
};

/// Hands out port bindings to parameters, in parameter order.
///
/// Each declared parameter takes one binding. A dynamic parameter
/// ([`DynInputs`] or [`DynOutputs`]) takes all the bindings beyond the
/// declared ones.
pub struct Bindings<B> {
    items: std::vec::IntoIter<B>,
    spare: usize,
}

impl<B> Bindings<B> {
    /// Wraps `items`, of which `declared` belong to declared parameters.
    pub(crate) fn new(items: Vec<B>, declared: usize) -> Self {
        Self {
            spare: items.len().saturating_sub(declared),
            items: items.into_iter(),
        }
    }

    /// Returns the binding for the next declared parameter.
    pub(crate) fn next(&mut self) -> Option<B> {
        self.items.next()
    }

    /// Returns the bindings for a dynamic parameter.
    ///
    /// Only the first call returns any.
    pub(crate) fn take(&mut self) -> Vec<B> {
        let spare = core::mem::take(&mut self.spare);
        self.items.by_ref().take(spare).collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.len() == 0
    }
}

/// Bound inputs, waiting to be handed to parameters.
///
/// [`Sample`] parameters also register their samplers here.
pub struct Views<W: WakeSink> {
    pub(crate) bindings: Bindings<InputBinding<W>>,
    pub(crate) samplers: Samplers,
}

/// Bound outputs, waiting to be handed to parameters.
///
/// Every output built from these shares one counter of failed writes.
pub struct Writers {
    pub(crate) bindings: Bindings<OutputBinding>,
    pub(crate) dropped: Arc<AtomicU32>,
    /// The instance's stream, for an `Rng` parameter.
    pub(crate) rng: Option<Rng>,
    /// The host's record table, for a `Records` parameter.
    pub(crate) records: Option<Arc<Records>>,
}
/// The parameter names that [`Param::append_defs`] reads from.
pub type ParamNames<'n> = core::slice::Iter<'n, &'static str>;

/// The ports a system's parameters take under one instance's config.
#[derive(Default)]
pub struct Defs {
    pub(crate) inputs: Vec<PortDef>,
    pub(crate) outputs: Vec<PortDef>,
    /// The names of the ports the system declares regardless of config.
    pub(crate) declared_inputs: Vec<Cow<'static, str>>,
    pub(crate) declared_outputs: Vec<Cow<'static, str>>,
    /// Whether any parameter is a [`Sample`]. Only a stepped system can take
    /// one.
    pub(crate) sampled: bool,
    /// Whether any parameter is an `Rng`.
    pub(crate) rng: bool,
    /// Whether any parameter is the [`Records`] table.
    pub(crate) records: bool,
}

impl Defs {
    /// Returns every port `S`'s parameters take under the config in `cx`.
    pub(crate) fn of<S: crate::fn_system::FnSignature>(cx: &DefCx<'_>) -> Result<Self, DefError> {
        let declared = Self::declared::<S>();
        Self {
            declared_inputs: names(&declared.inputs),
            declared_outputs: names(&declared.outputs),
            ..Self::default()
        }
        .walk::<S>(cx)
    }

    /// Returns the ports `S`'s parameters take regardless of config.
    pub(crate) fn declared<S: crate::fn_system::FnSignature>() -> Self {
        // PANIC Safety: a dynamic parameter only fails on ports named in the
        // config, and the empty context names none.
        Self::default()
            .walk::<S>(&DefCx::empty())
            .expect("no config ports")
    }

    /// Appends the ports `S`'s parameters take under the config in `cx`.
    fn walk<S: crate::fn_system::FnSignature>(mut self, cx: &DefCx<'_>) -> Result<Self, DefError> {
        S::Params::append_defs(&mut S::NAMES.iter(), cx, &mut self)?;
        Ok(self)
    }
}

fn names(ports: &[PortDef]) -> Vec<Cow<'static, str>> {
    ports.iter().map(|port| port.name.clone()).collect()
}

/// The per-cycle values a system fn's parameters can read besides ports.
pub struct Cycle<'a> {
    pub now: Timestamp,
    _cycle: core::marker::PhantomData<&'a ()>,
}

impl Cycle<'_> {
    pub(crate) fn new(now: Timestamp) -> Self {
        Self {
            now,
            _cycle: core::marker::PhantomData,
        }
    }
}

/// A type that a system fn can take as a parameter.
///
/// A system fn declares what it needs through its parameter types. Each
/// parameter declares the ports it uses, binds to them, and produces the
/// value the fn receives each cycle. For example, [`Input`] declares one
/// input port and hands the fn a `&mut Input`. Tuples of `Param`s are also
/// `Param`s.
///
/// `W` is the wake sink an input uses: `NoWake` on the cycle thread,
/// `Notifier` on a background thread. Outputs never wake anyone, because the
/// cycle thread drains an async system's outputs itself.
pub trait Param {
    /// The bound input, or `()` if the parameter has none.
    type In<W: WakeSink + Clone + 'static>;
    /// The bound output, or `()` if the parameter has none.
    type Out;
    /// The value the system fn receives each cycle.
    type Item<'a, W: WakeSink + Clone + 'static>
    where
        Self: 'a;

    /// Declares the ports this parameter takes under the config in `cx`.
    ///
    /// Takes the parameter's name from `names` and appends any ports to
    /// `defs`.
    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError>;

    /// Binds this parameter's input, if it has one, from `views`.
    fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W>;

    /// Binds this parameter's output, if it has one, from `writers`.
    fn bind_out(writers: &mut Writers) -> Self::Out;

    /// Returns the value passed to the system fn this cycle.
    fn get<'a, W: WakeSink + Clone + 'static>(
        input: &'a mut Self::In<W>,
        output: &'a mut Self::Out,
        cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W>;
}

/// Takes the name of the next non-tuple parameter.
pub(crate) fn param_name(names: &mut ParamNames<'_>) -> &'static str {
    // PANIC Safety: `#[system]` emits one name per parameter.
    names.next().expect("one name per parameter")
}

/// `Input<T>` and `Input<T, Notifier>` declare the same port.
///
/// The fn receives the wake sink that matches the thread it runs on.
impl<T: Record + 'static + ?Sized, D: WakeSink + 'static> Param for Input<T, D> {
    type In<W: WakeSink + Clone + 'static> = Input<T, W>;
    type Out = ();
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut Input<T, W>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.inputs.push(Input::<T>::def(cx, param_name(names)));
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W> {
        // PANIC Safety: the coordinator binds one list per declared input;
        // a mismatch is a coordinator bug.
        let binding = views.bindings.next().expect("one binding per input param");
        Input::new(binding.views)
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

/// A `Sample` declares the same port an `Input` would.
///
/// Instead of the input itself, the fn gets the newest record as of this
/// cycle. The sample is updated once per cycle, before the fn runs.
impl<T: Frame + Copy + 'static> Param for Sample<T> {
    type In<W: WakeSink + Clone + 'static> = Sample<T>;
    type Out = ();
    type Item<'a, W: WakeSink + Clone + 'static> = &'a Sample<T>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.inputs.push(Input::<T>::def(cx, param_name(names)));
        defs.sampled = true;
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W> {
        let input = <Input<T> as Param>::bind_in(views);
        let (sample, sampler) = Sample::bind(input);
        views.samplers.push(sampler);
        sample
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

/// An `Rng` draws from the target seed and the instance name. It takes no
/// port.
impl Param for Rng {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = Rng;
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut Rng;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        param_name(names);
        defs.rng = true;
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(writers: &mut Writers) -> Rng {
        // PANIC Safety: `fn_system::bind` refuses a set with an `Rng` and no
        // seed, and every other bind path refuses an `Rng` at its definition.
        writers.rng.clone().expect("a seeded stream")
    }

    fn get<'a, W: WakeSink + Clone + 'static>(
        _input: &'a mut (),
        output: &'a mut Rng,
        _cx: &mut Cycle<'a>,
    ) -> &'a mut Rng {
        output
    }
}

/// The host's record table: every record the system table knows, by name.
/// It takes no port.
impl Param for Records {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = Arc<Records>;
    type Item<'a, W: WakeSink + Clone + 'static> = &'a Records;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        param_name(names);
        defs.records = true;
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(writers: &mut Writers) -> Arc<Records> {
        // PANIC Safety: `fn_system::bind` and every async bind path pass the
        // table.
        writers.records.clone().expect("the host record table")
    }

    fn get<'a, W: WakeSink + Clone + 'static>(
        _input: &'a mut (),
        output: &'a mut Arc<Records>,
        _cx: &mut Cycle<'a>,
    ) -> &'a Records {
        output
    }
}

impl<T: Record + 'static + ?Sized> Param for Output<T> {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = Output<T>;
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut Output<T>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.outputs.push(Output::<T>::def(cx, param_name(names)));
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        // PANIC Safety: the coordinator binds one writer per declared output;
        // a mismatch is a coordinator bug.
        let binding = writers
            .bindings
            .next()
            .expect("one binding per output param");
        Output::counting(binding.writer, writers.dropped.clone())
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
        param_name(names);
        let ports = cx.dyn_input_defs(&defs.declared_inputs)?;
        defs.inputs.extend(ports);
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W> {
        Ports::bind(views.bindings.take())
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
        param_name(names);
        let ports = cx.dyn_output_defs(&defs.declared_outputs)?;
        defs.outputs.extend(ports);
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        DynOutputs::new(writers.bindings.take(), &writers.dropped)
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
        param_name(names);
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
    type Out = Log;
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut Log;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        _defs: &mut Defs,
    ) -> Result<(), DefError> {
        param_name(names);
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(_writers: &mut Writers) -> Log {
        Log
    }

    fn get<'a, W: WakeSink + Clone + 'static>(
        _input: &'a mut (),
        output: &'a mut Log,
        _cx: &mut Cycle<'a>,
    ) -> &'a mut Log {
        output
    }
}

/// Implements `Param` for a tuple of `Param`s.
///
/// Each associated type is the tuple of the elements' types, and each method
/// calls the elements in order.
macro_rules! impl_param_for_tuple {
    ($($P:ident $i:tt),*) => {
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

/// Implements `Param` for every prefix of the list: `()`, `(A,)`, `(A, B)`, ...
macro_rules! impl_param_for_tuples {
    ([$($done:ident $di:tt),*]) => {};
    ([$($done:ident $di:tt),*] $next:ident $ni:tt $(, $rest:ident $ri:tt)*) => {
        impl_param_for_tuple!($($done $di,)* $next $ni);
        impl_param_for_tuples!([$($done $di,)* $next $ni] $($rest $ri),*);
    };
}

impl_param_for_tuple!();
impl_param_for_tuples!([] A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11, M 12, N 13, O 14, P 15);
