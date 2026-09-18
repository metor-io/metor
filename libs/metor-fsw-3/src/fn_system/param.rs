//! The [`Param`] trait, one per `execute` parameter type, and its tuple impls.

use metor_fsw_3_ring::{NoWake, Notifier, WakeSink};
use metor_proto::types::Timestamp;

use crate::log::Log;
use crate::port::{DynInputs, DynOutputs, Input, Output};
use crate::record::Record;
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

/// Which wake endpoint a system's input ports carry.
///
/// Outputs never wake anyone: the cycle thread drains an async system's
/// outputs itself, so they stay [`NoWake`] under either wiring.
pub trait Wiring: 'static {
    type Sink: WakeSink + Clone;
}

/// The cycle thread's ports: a read finds a record or does not.
pub struct Cyclic;

/// A background thread's ports: a read parks until the cycle copies a record.
pub struct Woken;

impl Wiring for Cyclic {
    type Sink = NoWake;
}

impl Wiring for Woken {
    type Sink = Notifier;
}

/// The bindings a bundle binds from: one per declared port in order, then the
/// ports a dynamic bundle's config added.
pub struct Bindings<B> {
    declared: std::vec::IntoIter<B>,
    dynamic: Vec<B>,
}

impl<B> Bindings<B> {
    /// Splits `all` after the declared ports; the tail is the dynamic one's.
    pub(crate) fn new(mut all: Vec<B>, declared: usize) -> Self {
        let dynamic = all.split_off(declared);
        Self {
            declared: all.into_iter(),
            dynamic,
        }
    }

    pub(crate) fn next(&mut self) -> Option<B> {
        self.declared.next()
    }

    /// Takes every port the config added, for the one dynamic parameter.
    fn take_dynamic(&mut self) -> Vec<B> {
        core::mem::take(&mut self.dynamic)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.declared.len() == 0 && self.dynamic.is_empty()
    }
}

/// The input bindings a bundle binds from.
pub type Views<K> = Bindings<InputBinding<<K as Wiring>::Sink>>;
/// The output bindings a bundle binds from.
pub type Writers = Bindings<OutputBinding>;
/// The parameter names a bundle declares under, one per leaf parameter in order.
pub type Names<'n> = core::slice::Iter<'n, &'static str>;

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
/// For instance [`Input`] implements `Param` to bind an input port.
/// The goal is to let system fns define what parameters they need from the type system.
pub trait Param {
    /// Whether this parameter's input ports come from the config.
    const DYNAMIC_IN: bool = false;
    /// Whether this parameter's output ports come from the config.
    const DYNAMIC_OUT: bool = false;

    /// The bound input, or `()`.
    type In<K: Wiring>;
    /// The bound writer, or `()`.
    type Out;
    /// The value `execute` receives.
    type Item<'a, K: Wiring>
    where
        Self: 'a;

    /// Appends this parameter's port definition, if it has one, taking its name from `names`.
    fn append_defs(names: &mut Names<'_>, inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>);

    /// Returns this parameter's input (if one exists) by popping a view from `views`.
    fn bind_in<K: Wiring>(views: &mut Views<K>) -> Self::In<K>;

    /// Returns this parameter's output (if one exists) by popping a writer from `writers`.
    fn bind_out(writers: &mut Writers) -> Self::Out;

    /// Returns the value that will be passed into `execute`
    fn get<'a, K: Wiring>(
        input: &'a mut Self::In<K>,
        output: &'a mut Self::Out,
        cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, K>;
}

/// Takes the next leaf parameter's name.
fn name(names: &mut Names<'_>) -> &'static str {
    // PANIC Safety: `#[system]` emits one name per parameter.
    names.next().expect("one name per parameter")
}

/// Both `Input<T>` and `Input<T, Notifier>` name the same port; the wiring
/// picks which one a system is handed.
impl<T: Record + 'static + ?Sized, W: WakeSink + 'static> Param for Input<T, W> {
    type In<K: Wiring> = Input<T, K::Sink>;
    type Out = ();
    type Item<'a, K: Wiring> = &'a mut Input<T, K::Sink>;

    fn append_defs(names: &mut Names<'_>, inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        inputs.push(Input::<T>::def(name(names)));
    }

    fn bind_in<K: Wiring>(views: &mut Views<K>) -> Self::In<K> {
        // PANIC Safety: the coordinator binds one list per declared input and
        // validates alignment at build; a mismatch is a coordinator bug.
        let binding = views.next().expect("one binding per input param");
        Input::try_new(binding.views).expect("alignment checked at build")
    }

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a, K: Wiring>(
        input: &'a mut Self::In<K>,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, K> {
        input
    }
}

impl<T: Record + 'static + ?Sized> Param for Output<T> {
    type In<K: Wiring> = ();
    type Out = Output<T>;
    type Item<'a, K: Wiring> = &'a mut Output<T>;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>) {
        outputs.push(Output::<T>::def(name(names)));
    }

    fn bind_in<K: Wiring>(_views: &mut Views<K>) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        // PANIC Safety: the coordinator binds one writer per declared output and
        // validates alignment at build; a mismatch is a coordinator bug.
        let binding = writers.next().expect("one binding per output param");
        Output::try_new(binding.writer).expect("alignment checked at build")
    }

    fn get<'a, K: Wiring>(
        _input: &'a mut (),
        output: &'a mut Self::Out,
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, K> {
        output
    }
}

impl<W: WakeSink + 'static> Param for DynInputs<W> {
    const DYNAMIC_IN: bool = true;
    type In<K: Wiring> = DynInputs<K::Sink>;
    type Out = ();
    type Item<'a, K: Wiring> = &'a mut DynInputs<K::Sink>;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        name(names);
    }

    fn bind_in<K: Wiring>(views: &mut Views<K>) -> Self::In<K> {
        SystemInputs::bind(views.take_dynamic())
    }

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a, K: Wiring>(
        input: &'a mut Self::In<K>,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, K> {
        input
    }
}

impl Param for DynOutputs {
    const DYNAMIC_OUT: bool = true;
    type In<K: Wiring> = ();
    type Out = DynOutputs;
    type Item<'a, K: Wiring> = &'a mut DynOutputs;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        name(names);
    }

    fn bind_in<K: Wiring>(_views: &mut Views<K>) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        SystemOutputs::bind(writers.take_dynamic())
    }

    fn get<'a, K: Wiring>(
        _input: &'a mut (),
        output: &'a mut Self::Out,
        _cx: &mut Cycle<'a>,
    ) -> Self::Item<'a, K> {
        output
    }
}

impl Param for Timestamp {
    type In<K: Wiring> = ();
    type Out = ();
    type Item<'a, K: Wiring> = Timestamp;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        name(names);
    }

    fn bind_in<K: Wiring>(_views: &mut Views<K>) {}

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a, K: Wiring>(
        _input: &'a mut (),
        _output: &'a mut (),
        cx: &mut Cycle<'a>,
    ) -> Timestamp {
        cx.now
    }
}

impl Param for Log {
    type In<K: Wiring> = ();
    type Out = ();
    type Item<'a, K: Wiring> = &'a mut Log;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        name(names);
    }

    fn bind_in<K: Wiring>(_views: &mut Views<K>) {}

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a, K: Wiring>(
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
            const DYNAMIC_IN: bool = false $(|| $P::DYNAMIC_IN)*;
            const DYNAMIC_OUT: bool = false $(|| $P::DYNAMIC_OUT)*;
            type In<W: Wiring> = ($($P::In<W>,)*);
            type Out = ($($P::Out,)*);
            type Item<'a, W: Wiring> = ($($P::Item<'a, W>,)*) where Self: 'a;

            #[allow(unused_variables)]
            fn append_defs(names: &mut Names<'_>, inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>) {
                $( $P::append_defs(names, inputs, outputs); )*
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_in<W: Wiring>(views: &mut Views<W>) -> Self::In<W> {
                ($( $P::bind_in::<W>(views), )*)
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_out(writers: &mut Writers) -> Self::Out {
                ($( $P::bind_out(writers), )*)
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn get<'a, W: Wiring>(input: &'a mut Self::In<W>, output: &'a mut Self::Out, cx: &mut Cycle<'a>) -> Self::Item<'a, W> {
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
