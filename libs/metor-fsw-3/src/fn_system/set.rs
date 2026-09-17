//! The bundles a fn system binds its parameters through.

use metor_fsw_3_ring::{NoWake, View, Writer};
use metor_proto_wkt::LogEvent;

use crate::log::LogPort;
use crate::port::Output;
use crate::system::{PortDef, SystemInputs, SystemOutputs};

use super::SystemFn;
use super::param::Param;

/// LOG_PORT is the name of the port that receives log events.
pub const LOG_PORT: &str = "log";

/// An `InSet` is the bound inputs of a fn system's parameters.
pub struct InSet<S: SystemFn>(pub(crate) <S::Params as Param>::In);

/// An `OutSet` is the bound outputs of a fn system's parameters, plus its `log`.
pub struct OutSet<S: SystemFn> {
    pub(crate) outs: <S::Params as Param>::Out,
    pub(crate) log: LogPort,
}

impl<S: SystemFn> SystemInputs for InSet<S> {
    fn defs() -> Vec<PortDef> {
        let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
        S::Params::append_defs(&mut S::NAMES.iter(), &mut inputs, &mut outputs);
        inputs
    }

    fn bind(views: Vec<Vec<View<NoWake>>>) -> Self {
        let mut views = views.into_iter();
        let ins = S::Params::bind_in(&mut views);
        // PANIC Safety: the coordinator binds exactly the declared ports.
        assert!(views.next().is_none(), "more view lists than input params");
        Self(ins)
    }
}

impl<S: SystemFn> SystemOutputs for OutSet<S> {
    fn defs() -> Vec<PortDef> {
        let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
        S::Params::append_defs(&mut S::NAMES.iter(), &mut inputs, &mut outputs);
        outputs.push(Output::<LogEvent>::def(LOG_PORT));
        outputs
    }

    fn bind(writers: Vec<Writer<NoWake>>) -> Self {
        let mut writers = writers.into_iter();
        let outs = S::Params::bind_out(&mut writers);
        // PANIC Safety: the coordinator binds exactly the declared ports, and
        // LogEvent needs no alignment.
        let log = writers.next().expect("one writer for the log output");
        let log = LogPort::new(Output::try_new(log).expect("byte alignment"));
        assert!(writers.next().is_none(), "more writers than output params");
        Self { outs, log }
    }
}
