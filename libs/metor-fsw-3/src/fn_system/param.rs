//! The [`Param`] trait, one per `execute` parameter type, and its tuple impls.

use metor_proto::types::Timestamp;

use crate::log::Log;
use crate::port::{DynInputs, DynOutputs, Input, Output};
use crate::record::Record;
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

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
pub type Views = Bindings<InputBinding>;
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
    type In;
    /// The bound writer, or `()`.
    type Out;
    /// The value `execute` receives.
    type Item<'a>
    where
        Self: 'a;

    /// Appends this parameter's port definition, if it has one, taking its name from `names`.
    fn append_defs(names: &mut Names<'_>, inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>);

    /// Returns this parameter's input (if one exists) by popping a view from `views`.
    fn bind_in(views: &mut Views) -> Self::In;

    /// Returns this parameter's output (if one exists) by popping a writer from `writers`.
    fn bind_out(writers: &mut Writers) -> Self::Out;

    /// Returns the value that will be passed into `execute`
    fn get<'a>(
        input: &'a mut Self::In,
        output: &'a mut Self::Out,
        cx: &mut Cycle<'a>,
    ) -> Self::Item<'a>;
}

/// Takes the next leaf parameter's name.
fn name(names: &mut Names<'_>) -> &'static str {
    // PANIC Safety: `#[system]` emits one name per parameter.
    names.next().expect("one name per parameter")
}

impl<T: Record + 'static> Param for Input<T> {
    type In = Input<T>;
    type Out = ();
    type Item<'a> = &'a mut Input<T>;

    fn append_defs(names: &mut Names<'_>, inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        inputs.push(Input::<T>::def(name(names)));
    }

    fn bind_in(views: &mut Views) -> Self::In {
        // PANIC Safety: the coordinator binds one list per declared input and
        // validates alignment at build; a mismatch is a coordinator bug.
        let binding = views.next().expect("one binding per input param");
        Input::try_new(binding.views).expect("alignment checked at build")
    }

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a>(
        input: &'a mut Self::In,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> &'a mut Input<T> {
        input
    }
}

impl<T: Record + 'static> Param for Output<T> {
    type In = ();
    type Out = Output<T>;
    type Item<'a> = &'a mut Output<T>;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>) {
        outputs.push(Output::<T>::def(name(names)));
    }

    fn bind_in(_views: &mut Views) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        // PANIC Safety: the coordinator binds one writer per declared output and
        // validates alignment at build; a mismatch is a coordinator bug.
        let binding = writers.next().expect("one binding per output param");
        Output::try_new(binding.writer).expect("alignment checked at build")
    }

    fn get<'a>(
        _input: &'a mut (),
        output: &'a mut Self::Out,
        _cx: &mut Cycle<'a>,
    ) -> &'a mut Output<T> {
        output
    }
}

impl Param for DynInputs {
    const DYNAMIC_IN: bool = true;
    type In = DynInputs;
    type Out = ();
    type Item<'a> = &'a mut DynInputs;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        name(names);
    }

    fn bind_in(views: &mut Views) -> Self::In {
        SystemInputs::bind(views.take_dynamic())
    }

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a>(
        input: &'a mut Self::In,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> &'a mut DynInputs {
        input
    }
}

impl Param for DynOutputs {
    const DYNAMIC_OUT: bool = true;
    type In = ();
    type Out = DynOutputs;
    type Item<'a> = &'a mut DynOutputs;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        name(names);
    }

    fn bind_in(_views: &mut Views) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        SystemOutputs::bind(writers.take_dynamic())
    }

    fn get<'a>(
        _input: &'a mut (),
        output: &'a mut Self::Out,
        _cx: &mut Cycle<'a>,
    ) -> &'a mut DynOutputs {
        output
    }
}

impl Param for Timestamp {
    type In = ();
    type Out = ();
    type Item<'a> = Timestamp;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        name(names);
    }

    fn bind_in(_views: &mut Views) {}

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a>(_input: &'a mut (), _output: &'a mut (), cx: &mut Cycle<'a>) -> Timestamp {
        cx.now
    }
}

impl Param for Log {
    type In = ();
    type Out = ();
    type Item<'a> = &'a mut Log;

    fn append_defs(names: &mut Names<'_>, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        name(names);
    }

    fn bind_in(_views: &mut Views) {}

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a>(_input: &'a mut (), _output: &'a mut (), cx: &mut Cycle<'a>) -> &'a mut Log {
        cx.take_log()
    }
}

/// A tuple of `Param`s is a `Param`: each half is the tuple of the elements' halves.
macro_rules! impl_param_for_tuple {
    ($(($P:ident, $i:tt)),*) => {
        impl<$($P: Param),*> Param for ($($P,)*) {
            const DYNAMIC_IN: bool = false $(|| $P::DYNAMIC_IN)*;
            const DYNAMIC_OUT: bool = false $(|| $P::DYNAMIC_OUT)*;
            type In = ($($P::In,)*);
            type Out = ($($P::Out,)*);
            type Item<'a> = ($($P::Item<'a>,)*) where Self: 'a;

            #[allow(unused_variables)]
            fn append_defs(names: &mut Names<'_>, inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>) {
                $( $P::append_defs(names, inputs, outputs); )*
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_in(views: &mut Views) -> Self::In {
                ($( $P::bind_in(views), )*)
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn bind_out(writers: &mut Writers) -> Self::Out {
                ($( $P::bind_out(writers), )*)
            }

            #[allow(unused_variables, clippy::unused_unit)]
            fn get<'a>(input: &'a mut Self::In, output: &'a mut Self::Out, cx: &mut Cycle<'a>) -> Self::Item<'a> {
                ($( $P::get(&mut input.$i, &mut output.$i, cx), )*)
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
