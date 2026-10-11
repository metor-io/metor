//! The [`Param`] trait, one per system fn parameter type, and its tuple impls.

use std::borrow::Cow;

use metor_fsw_3_ring::WakeSink;
use metor_proto::types::Timestamp;

use crate::channel::{ABORT_PORT, Abort, AbortRequest};
use crate::def::{DefCx, DefError};
use crate::frame::Frame;
use crate::log::Log;
use crate::port::{DynInputs, DynOutputs, Input, Output, Sample, Samplers};
use crate::record::Record;
use crate::sequence::Seq;
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

/// The bindings a bundle binds from, in parameter order: each declared
/// parameter takes one, and the one dynamic parameter that bound any takes
/// the rest.
pub struct Bindings<B> {
    items: std::vec::IntoIter<B>,
    spare: usize,
}

impl<B> Bindings<B> {
    /// Hands out `items`, of which `declared` belong to declared parameters.
    pub(crate) fn new(items: Vec<B>, declared: usize) -> Self {
        Self {
            spare: items.len().saturating_sub(declared),
            items: items.into_iter(),
        }
    }

    /// The one binding the next declared parameter takes.
    pub(crate) fn next(&mut self) -> Option<B> {
        self.items.next()
    }

    /// Every binding the next dynamic parameter takes.
    pub(crate) fn take(&mut self) -> Vec<B> {
        let spare = core::mem::take(&mut self.spare);
        self.items.by_ref().take(spare).collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.len() == 0
    }
}

/// "Views" are bound inputs, plus the samplers [`Sample`] parameters push.
pub struct Views<W: WakeSink> {
    pub(crate) bindings: Bindings<InputBinding<W>>,
    pub(crate) samplers: Samplers,
}

/// "Writers" are bound outputs
pub type Writers = Bindings<OutputBinding>;
/// A bundles parameter names
pub type ParamNames<'n> = core::slice::Iter<'n, &'static str>;

/// The ports a system's parameters take under one instance's config.
#[derive(Default)]
pub struct Defs {
    pub(crate) inputs: Vec<PortDef>,
    pub(crate) outputs: Vec<PortDef>,
    /// The ports the type declares whatever its config.
    pub(crate) declared_inputs: Vec<Cow<'static, str>>,
    pub(crate) declared_outputs: Vec<Cow<'static, str>>,
    /// Whether a parameter is a [`Sample`], which only a stepped system takes.
    pub(crate) sampled: bool,
}

impl Defs {
    /// Every port a system's parameters take under `cx`, under the names it gave them.
    pub(crate) fn of<S: crate::fn_system::Ports>(cx: &DefCx<'_>) -> Result<Self, DefError> {
        let declared = Self::declared::<S>();
        Self::walk::<S>(
            cx,
            Self {
                declared_inputs: names(&declared.inputs),
                declared_outputs: names(&declared.outputs),
                ..Self::default()
            },
        )
    }

    /// The ports a system's parameters take whatever its config.
    pub(crate) fn declared<S: crate::fn_system::Ports>() -> Self {
        // PANIC Safety: a dynamic parameter refuses only config ports, and
        // the empty context has none.
        Self::walk::<S>(&DefCx::empty(), Self::default()).expect("no config ports")
    }

    fn walk<S: crate::fn_system::Ports>(cx: &DefCx<'_>, mut defs: Self) -> Result<Self, DefError> {
        S::Params::append_defs(&mut S::NAMES.iter(), cx, &mut defs)?;
        Ok(defs)
    }
}

fn names(ports: &[PortDef]) -> Vec<Cow<'static, str>> {
    ports.iter().map(|port| port.name.clone()).collect()
}

/// `Cycle` is what a system fn's parameters read beyond their ports.
pub struct Cycle<'a> {
    pub now: Timestamp,
    log: Option<&'a mut Log>,
    seq: Option<&'a Seq<'static>>,
}

impl<'a> Cycle<'a> {
    pub(crate) fn new(now: Timestamp, log: &'a mut Log) -> Self {
        Self {
            now,
            log: Some(log),
            seq: None,
        }
    }

    /// A sequence's cycle, which hands its [`Seq`] to the parameter taking it.
    ///
    /// `'a` borrows the locals of the body future the step polls, so it
    /// ends no later than that body.
    pub(crate) fn with_seq(now: Timestamp, log: &'a mut Log, seq: &'a Seq<'static>) -> Self {
        Self {
            now,
            log: Some(log),
            seq: Some(seq),
        }
    }

    /// The sequence's handle, branded with this cycle's lifetime.
    pub fn seq(&self) -> &'a Seq<'a> {
        // PANIC Safety: `#[system]` rejects a `Seq` parameter, so only a
        // sequence, whose step builds its cycle `with_seq`, reaches this.
        let seq = self
            .seq
            .expect("a `Seq` parameter only on a `#[sequence]` fn");
        // SAFETY: the brand is phantom, so the layout is unchanged. Through
        // the result, a condition links only if it outlives `'a`, which ends
        // no later than the body (`with_seq`). The step checks redlines only
        // while it holds that body: `advance` walks behind `run.as_mut()`,
        // and `run` is `None` from the body's drop on, in `execute` and in
        // `fault` alike. `Seq`'s `Drop` clears the back-pointers of nodes
        // still linked without calling a condition.
        unsafe { &*core::ptr::from_ref(seq).cast::<Seq<'a>>() }
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
    /// The value the system fn receives.
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

    /// Returns the value passed to the system fn.
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
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.inputs.push(Input::<T>::def(cx, name(names)));
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

/// A `Sample` reads the port an `Input` would, taking it once per cycle.
impl<T: Frame + Copy + 'static> Param for Sample<T> {
    type In<W: WakeSink + Clone + 'static> = Sample<T>;
    type Out = ();
    type Item<'a, W: WakeSink + Clone + 'static> = &'a Sample<T>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.inputs.push(Input::<T>::def(cx, name(names)));
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

impl<T: Record + 'static + ?Sized> Param for Output<T> {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = Output<T>;
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut Output<T>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.outputs.push(Output::<T>::def(cx, name(names)));
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(_views: &mut Views<W>) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        // PANIC Safety: the coordinator binds one writer per declared output;
        // a mismatch is a coordinator bug.
        let binding = writers.next().expect("one binding per output param");
        Output::new(binding.writer)
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
        let ports = crate::port::dyn_input_defs(cx, &defs.declared_inputs)?;
        defs.inputs.extend(ports);
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W> {
        SystemInputs::bind(views.bindings.take())
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
        let ports = crate::port::dyn_output_defs(cx, &defs.declared_outputs)?;
        defs.outputs.extend(ports);
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

/// An [`Abort`] reads the `abort` input, whatever the parameter is called.
impl<D: WakeSink + 'static> Param for Abort<D> {
    type In<W: WakeSink + Clone + 'static> = Abort<W>;
    type Out = ();
    type Item<'a, W: WakeSink + Clone + 'static> = &'a mut Abort<W>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        name(names);
        defs.inputs.push(Input::<AbortRequest>::def(cx, ABORT_PORT));
        Ok(())
    }

    fn bind_in<W: WakeSink + Clone + 'static>(views: &mut Views<W>) -> Self::In<W> {
        Abort::new(<Input<AbortRequest> as Param>::bind_in(views))
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

impl Param for Seq<'static> {
    type In<W: WakeSink + Clone + 'static> = ();
    type Out = ();
    type Item<'a, W: WakeSink + Clone + 'static> = &'a Seq<'a>;

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
    ) -> &'a Seq<'a> {
        cx.seq()
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
