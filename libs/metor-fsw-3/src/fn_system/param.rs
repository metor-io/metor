//! The [`Param`] trait, which defines what a system fn can take as a
//! parameter.

use core::sync::atomic::AtomicU32;
use std::borrow::Cow;
use std::sync::Arc;

use metor_fsw_3_ring::WakeSink;
use metor_proto::types::Timestamp;

use crate::{
    Rng,
    channel::{ABORT_PORT, Abort, AbortRequest},
    def::{DefCx, DefError, Records},
    frame::Frame,
    io::{Cqe, GrantDef, IO_CQ, IO_SQ, Io, IoPorts, Sqe},
    log::Log,
    port::{DynInputs, DynOutputs, Input, InputWake, Output, Sample, SampledInput},
    record::Record,
    sequence::Seq,
    system::{InputBinding, OutputBinding, PortDef},
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
pub struct Views<W: WakeSink> {
    pub(crate) bindings: Bindings<InputBinding<W>>,
}

impl<W: WakeSink> Views<W> {
    /// Wraps `inputs`, of which `declared` belong to declared parameters.
    pub(crate) fn new(inputs: Vec<InputBinding<W>>, declared: usize) -> Self {
        Self {
            bindings: Bindings::new(inputs, declared),
        }
    }
}

/// Bound outputs, waiting to be handed to parameters, and the instance they
/// belong to.
///
/// Every output built from these shares one counter of failed writes. A
/// `Cap` takes its handle from `io`.
pub struct Writers {
    pub(crate) bindings: Bindings<OutputBinding>,
    pub(crate) dropped: Arc<AtomicU32>,
    pub(crate) instance: Instance,
    pub(crate) io: Option<Io>,
}

impl Writers {
    /// Wraps `outputs`, of which `declared` belong to declared parameters.
    pub(crate) fn new(
        outputs: Vec<OutputBinding>,
        declared: usize,
        instance: Instance,
        io: Option<Io>,
    ) -> Self {
        Self {
            bindings: Bindings::new(outputs, declared),
            dropped: Arc::default(),
            instance,
            io,
        }
    }
}

/// The id, namespace, seed, and record table of the system instance being
/// bound.
#[derive(Clone, Debug, Default)]
pub(crate) struct Instance {
    pub(crate) id: String,
    pub(crate) namespace: Option<String>,
    /// The target seed.
    pub(crate) seed: u64,
    /// Every record the system table knows about.
    pub(crate) records: Arc<Records>,
}

impl Instance {
    pub(crate) fn from_make_cx(cx: &crate::coordinator::MakeCx<'_>) -> Self {
        Self {
            id: cx.id.to_string(),
            namespace: cx.namespace.map(str::to_string),
            seed: cx.seed,
            records: cx.records.clone(),
        }
    }

    /// The stream the `Rng` parameter `param` draws, named
    /// `{namespace}.{id}.{param}`.
    fn rng(&self, param: &str) -> Rng {
        let name = DefCx::path(self.namespace.as_deref(), &self.id, param);
        Rng::seeded(self.seed, &name)
    }
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
    /// The `Cap` parameters, in order.
    pub(crate) grants: Vec<GrantDef>,
    /// Where `append_io` put the queue pair's ports.
    pub(crate) io: Option<IoPorts>,
    /// Whether any parameter is an `Rng`.
    pub(crate) rng: bool,
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
        self.append_io(cx);
        Ok(self)
    }

    /// Appends `io_cq` and `io_sq`, for a `Cap`, after the parameters' own
    /// ports, and records where they are.
    fn append_io(&mut self, cx: &DefCx<'_>) {
        if self.grants.is_empty() {
            return;
        }
        self.io = Some(IoPorts {
            cq: self.inputs.len(),
            sq: self.outputs.len(),
        });
        self.inputs.push(Input::<Cqe>::def(cx, IO_CQ));
        self.outputs.push(Output::<Sqe>::def(cx, IO_SQ));
    }

    /// The inputs the parameters bind, which are all but `io_cq`.
    pub(crate) fn param_inputs(&self) -> usize {
        self.inputs.len() - usize::from(self.io.is_some())
    }

    /// The outputs the parameters bind, which are all but `io_sq`.
    pub(crate) fn param_outputs(&self) -> usize {
        self.outputs.len() - usize::from(self.io.is_some())
    }
}

fn names(ports: &[PortDef]) -> Vec<Cow<'static, str>> {
    ports.iter().map(|port| port.name.clone()).collect()
}

/// The per-cycle values a system fn's parameters can read besides ports.
///
/// These are the cycle time and, for a sequence, its [`Seq`].
pub struct Cycle<'a> {
    pub now: Timestamp,
    seq: Option<&'a Seq<'static>>,
}

impl<'a> Cycle<'a> {
    pub(crate) fn new(now: Timestamp) -> Self {
        Self { now, seq: None }
    }

    /// Creates a cycle for a sequence.
    ///
    /// The parameter that takes a [`Seq`] receives `seq`. `'a` borrows locals
    /// of the body future the step polls, so it ends no later than that body.
    pub(crate) fn with_seq(now: Timestamp, seq: &'a Seq<'static>) -> Self {
        Self {
            now,
            seq: Some(seq),
        }
    }

    /// Returns the sequence's handle as a `Seq<'a>`.
    ///
    /// Redlines linked through it may only borrow what outlives `'a`.
    pub fn seq(&self) -> &'a Seq<'a> {
        // PANIC Safety: `#[system]` rejects a `Seq` parameter, so only a
        // sequence, whose step builds its cycle `with_seq`, reaches this.
        let seq = self
            .seq
            .expect("a `Seq` parameter only on a `#[sequence]` fn");
        // SAFETY: the lifetime parameter is phantom, so the layout does not
        // change. Through the result, a condition can only be linked if it
        // outlives `'a`, which ends no later than the body (see `with_seq`).
        // The step checks redlines only while it holds that body: `advance`
        // walks them behind `run.as_mut()`, and `run` is `None` from the
        // moment the body is dropped, in both `execute` and `fault`. `Seq`'s
        // `Drop` clears the back-pointers of nodes that are still linked,
        // without calling any condition.
        unsafe { &*core::ptr::from_ref(seq).cast::<Seq<'a>>() }
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
    type In<W: InputWake>;
    /// The bound output, or `()` if the parameter has none.
    type Out;
    /// The value the system fn receives each cycle.
    type Item<'a, W: InputWake>
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
    fn bind_in<W: InputWake>(views: &mut Views<W>) -> Self::In<W>;

    /// Binds this parameter's output, if it has one, from `writers`.
    ///
    /// Takes the parameter's name from `names`.
    fn bind_out(names: &mut ParamNames<'_>, writers: &mut Writers) -> Result<Self::Out, MakeError>;

    /// Returns the value passed to the system fn this cycle.
    fn get<'a, W: InputWake>(
        input: &'a mut Self::In<W>,
        output: &'a mut Self::Out,
        cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W>;

    /// Refreshes this parameter's [`Sample`]s for the cycle, before `get`.
    fn refresh<W: InputWake>(_input: &mut Self::In<W>) {}
}

/// Takes the name of the next non-tuple parameter.
pub(crate) fn param_name(names: &mut ParamNames<'_>) -> &'static str {
    // PANIC Safety: `#[system]` emits one name per parameter.
    names.next().expect("one name per parameter")
}

/// `Input<T>` and `Input<T, Notifier>` declare the same port.
///
/// The fn receives the wake sink that matches the thread it runs on.
impl<T: Record + 'static + ?Sized, D: InputWake> Param for Input<T, D> {
    type In<W: InputWake> = Input<T, W>;
    type Out = ();
    type Item<'a, W: InputWake> = &'a mut Input<T, W>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.inputs.push(Input::<T>::def(cx, param_name(names)));
        Ok(())
    }

    fn bind_in<W: InputWake>(views: &mut Views<W>) -> Self::In<W> {
        // PANIC Safety: the coordinator binds one list per declared input;
        // a mismatch is a coordinator bug.
        let binding = views.bindings.next().expect("one binding per input param");
        Input::new(binding.views)
    }

    fn bind_out(names: &mut ParamNames<'_>, _writers: &mut Writers) -> Result<(), MakeError> {
        param_name(names);
        Ok(())
    }

    fn get<'a, W: InputWake>(
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
    type In<W: InputWake> = SampledInput<T, W>;
    type Out = ();
    type Item<'a, W: InputWake> = &'a Sample<T>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.inputs.push(Input::<T>::def(cx, param_name(names)));
        defs.sampled = true;
        Ok(())
    }

    fn bind_in<W: InputWake>(views: &mut Views<W>) -> Self::In<W> {
        SampledInput::new(<Input<T> as Param>::bind_in(views))
    }

    fn bind_out(names: &mut ParamNames<'_>, _writers: &mut Writers) -> Result<(), MakeError> {
        param_name(names);
        Ok(())
    }

    fn get<'a, W: InputWake>(
        input: &'a mut Self::In<W>,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W> {
        &input.sample
    }

    fn refresh<W: InputWake>(input: &mut Self::In<W>) {
        input.refresh();
    }
}

/// An `Rng` draws from the target seed and its instance and parameter
/// names. It takes no port.
impl Param for Rng {
    type In<W: InputWake> = ();
    type Out = Rng;
    type Item<'a, W: InputWake> = &'a mut Rng;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        param_name(names);
        defs.rng = true;
        Ok(())
    }

    fn bind_in<W: InputWake>(_views: &mut Views<W>) {}

    fn bind_out(names: &mut ParamNames<'_>, writers: &mut Writers) -> Result<Rng, MakeError> {
        Ok(writers.instance.rng(param_name(names)))
    }

    fn get<'a, W: InputWake>(
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
    type In<W: InputWake> = ();
    type Out = Arc<Records>;
    type Item<'a, W: InputWake> = &'a Records;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        _defs: &mut Defs,
    ) -> Result<(), DefError> {
        param_name(names);
        Ok(())
    }

    fn bind_in<W: InputWake>(_views: &mut Views<W>) {}

    fn bind_out(
        names: &mut ParamNames<'_>,
        writers: &mut Writers,
    ) -> Result<Arc<Records>, MakeError> {
        param_name(names);
        Ok(writers.instance.records.clone())
    }

    fn get<'a, W: InputWake>(
        _input: &'a mut (),
        output: &'a mut Arc<Records>,
        _cx: &mut Cycle<'a>,
    ) -> &'a Records {
        output
    }
}

impl<T: Record + 'static + ?Sized> Param for Output<T> {
    type In<W: InputWake> = ();
    type Out = Output<T>;
    type Item<'a, W: InputWake> = &'a mut Output<T>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        defs.outputs.push(Output::<T>::def(cx, param_name(names)));
        Ok(())
    }

    fn bind_in<W: InputWake>(_views: &mut Views<W>) {}

    fn bind_out(names: &mut ParamNames<'_>, writers: &mut Writers) -> Result<Self::Out, MakeError> {
        param_name(names);
        // PANIC Safety: the coordinator binds one writer per declared output;
        // a mismatch is a coordinator bug.
        let binding = writers
            .bindings
            .next()
            .expect("one binding per output param");
        Ok(Output::counting(binding.writer, writers.dropped.clone()))
    }

    fn get<'a, W: InputWake>(
        _input: &'a mut (),
        output: &'a mut Self::Out,
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W> {
        output
    }
}

impl<D: InputWake> Param for DynInputs<D> {
    type In<W: InputWake> = DynInputs<W>;
    type Out = ();
    type Item<'a, W: InputWake> = &'a mut DynInputs<W>;

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

    fn bind_in<W: InputWake>(views: &mut Views<W>) -> Self::In<W> {
        DynInputs::new(views.bindings.take())
    }

    fn bind_out(names: &mut ParamNames<'_>, _writers: &mut Writers) -> Result<(), MakeError> {
        param_name(names);
        Ok(())
    }

    fn get<'a, W: InputWake>(
        input: &'a mut Self::In<W>,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W> {
        input
    }
}

impl Param for DynOutputs {
    type In<W: InputWake> = ();
    type Out = DynOutputs;
    type Item<'a, W: InputWake> = &'a mut DynOutputs;

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

    fn bind_in<W: InputWake>(_views: &mut Views<W>) {}

    fn bind_out(names: &mut ParamNames<'_>, writers: &mut Writers) -> Result<Self::Out, MakeError> {
        param_name(names);
        Ok(DynOutputs::new(writers.bindings.take(), &writers.dropped))
    }

    fn get<'a, W: InputWake>(
        _input: &'a mut (),
        output: &'a mut Self::Out,
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, W> {
        output
    }
}

impl Param for Timestamp {
    type In<W: InputWake> = ();
    type Out = ();
    type Item<'a, W: InputWake> = Timestamp;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        _defs: &mut Defs,
    ) -> Result<(), DefError> {
        param_name(names);
        Ok(())
    }

    fn bind_in<W: InputWake>(_views: &mut Views<W>) {}

    fn bind_out(names: &mut ParamNames<'_>, _writers: &mut Writers) -> Result<(), MakeError> {
        param_name(names);
        Ok(())
    }

    fn get<'a, W: InputWake>(
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
        param_name(names);
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
        param_name(names);
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
    type In<W: InputWake> = ();
    type Out = Log;
    type Item<'a, W: InputWake> = &'a mut Log;

    fn append_defs(
        names: &mut ParamNames<'_>,
        _cx: &DefCx<'_>,
        _defs: &mut Defs,
    ) -> Result<(), DefError> {
        param_name(names);
        Ok(())
    }

    fn bind_in<W: InputWake>(_views: &mut Views<W>) {}

    fn bind_out(names: &mut ParamNames<'_>, _writers: &mut Writers) -> Result<Log, MakeError> {
        param_name(names);
        Ok(Log)
    }

    fn get<'a, W: InputWake>(
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
            type In<W: InputWake> = ($($P::In<W>,)*);
            type Out = ($($P::Out,)*);
            type Item<'a, W: InputWake> = ($($P::Item<'a, W>,)*) where Self: 'a;

            #[allow(unused_variables)]
            fn append_defs(names: &mut ParamNames<'_>, cx: &DefCx<'_>, defs: &mut Defs) -> Result<(), DefError> {
                $( $P::append_defs(names, cx, defs)?; )*
                Ok(())
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_in<W: InputWake>(views: &mut Views<W>) -> Self::In<W> {
                ($( $P::bind_in::<W>(views), )*)
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_out(names: &mut ParamNames<'_>, writers: &mut Writers) -> Result<Self::Out, MakeError> {
                Ok(($( $P::bind_out(names, writers)?, )*))
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn get<'a, W: InputWake>(input: &'a mut Self::In<W>, output: &'a mut Self::Out, cx: &mut Cycle<'a>) -> Self::Item<'a, W> {
                ($( $P::get::<W>(&mut input.$i, &mut output.$i, cx), )*)
            }

            #[allow(unused_variables)]
            fn refresh<W: InputWake>(input: &mut Self::In<W>) {
                $( $P::refresh::<W>(&mut input.$i); )*
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
