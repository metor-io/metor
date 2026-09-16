//! The [`Param`] trait, one per `execute` parameter type.

use metor_fsw_3_ring::{NoWake, View, Writer};
use metor_proto::types::Timestamp;

use crate::log::Log;
use crate::port::{Input, Output};
use crate::record::Record;
use crate::system::PortDef;

/// The view lists a bundle binds from, one per input port in order.
pub type Views = std::vec::IntoIter<Vec<View<NoWake>>>;
/// The writers a bundle binds from, one per output port in order.
pub type Writers = std::vec::IntoIter<Writer<NoWake>>;

/// A `Cycle` is what one `execute` call receives beyond its ports: the time and the log.
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

/// A `Param` is one `execute` parameter: what it declares, how it binds, and what it hands over each cycle.
pub trait Param {
    /// The bound input, or `()`.
    type In;
    /// The bound writer, or `()`.
    type Out;
    /// The value `execute` receives.
    type Item<'a>
    where
        Self: 'a;

    /// Declares this parameter's port, if it has one, under `name`.
    fn defs(name: &'static str, inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>);

    /// Takes this parameter's view list, if it is an input, from the front of `views`.
    fn bind_in(views: &mut Views) -> Self::In;

    /// Takes this parameter's writer, if it is an output, from the front of `writers`.
    fn bind_out(writers: &mut Writers) -> Self::Out;

    /// Produces the value for one cycle.
    fn get<'a>(
        input: &'a mut Self::In,
        output: &'a mut Self::Out,
        cx: &mut Cycle<'a>,
    ) -> Self::Item<'a>;
}

impl<T: Record + 'static> Param for Input<T> {
    type In = Input<T>;
    type Out = ();
    type Item<'a> = &'a mut Input<T>;

    fn defs(name: &'static str, inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {
        inputs.push(Input::<T>::def(name));
    }

    fn bind_in(views: &mut Views) -> Self::In {
        // PANIC Safety: the coordinator binds one list per declared input and
        // validates alignment at build; a mismatch is a coordinator bug.
        let views = views.next().expect("one view list per input param");
        Input::try_new(views).expect("alignment checked at build")
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

    fn defs(name: &'static str, _inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>) {
        outputs.push(Output::<T>::def(name));
    }

    fn bind_in(_views: &mut Views) {}

    fn bind_out(writers: &mut Writers) -> Self::Out {
        // PANIC Safety: the coordinator binds one writer per declared output and
        // validates alignment at build; a mismatch is a coordinator bug.
        let writer = writers.next().expect("one writer per output param");
        Output::try_new(writer).expect("alignment checked at build")
    }

    fn get<'a>(
        _input: &'a mut (),
        output: &'a mut Self::Out,
        _cx: &mut Cycle<'a>,
    ) -> &'a mut Output<T> {
        output
    }
}

impl Param for Timestamp {
    type In = ();
    type Out = ();
    type Item<'a> = Timestamp;

    fn defs(_name: &'static str, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {}

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

    fn defs(_name: &'static str, _inputs: &mut Vec<PortDef>, _outputs: &mut Vec<PortDef>) {}

    fn bind_in(_views: &mut Views) {}

    fn bind_out(_writers: &mut Writers) {}

    fn get<'a>(_input: &'a mut (), _output: &'a mut (), cx: &mut Cycle<'a>) -> &'a mut Log {
        cx.take_log()
    }
}
