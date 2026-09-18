//! The bundles a fn system binds its parameters through.

use metor_proto_wkt::LogEvent;

use crate::log::LogPort;
use crate::port::Output;
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

use std::borrow::Cow;

use super::Ports;
use super::param::{Bindings, Cyclic, Defs, Param, Wiring};

/// LOG_PORT is the name of the port that receives log events.
pub const LOG_PORT: &str = "log";

/// An `InSet` is the bound inputs of a fn system's parameters.
pub struct InSet<S: Ports, K: Wiring = Cyclic>(pub(crate) <S::Params as Param>::In<K>);

/// An `OutSet` is the bound outputs of a fn system's parameters, plus its `log`.
pub struct OutSet<S: Ports> {
    pub(crate) outs: <S::Params as Param>::Out,
    pub(crate) log: LogPort,
}

impl<S: Ports, K: Wiring> SystemInputs<K::Sink> for InSet<S, K> {
    fn defs() -> Vec<PortDef> {
        Defs::of::<S>().inputs
    }

    fn dynamic() -> Option<Cow<'static, str>> {
        Defs::of::<S>().dynamic_inputs
    }

    fn bind(inputs: Vec<InputBinding<K::Sink>>) -> Self {
        let declared = <Self as SystemInputs<K::Sink>>::defs().len();
        // PANIC Safety: the coordinator binds the declared ports, then the ones
        // it added to a dynamic bundle.
        assert!(inputs.len() >= declared, "fewer bindings than input params");
        let mut views = Bindings::new(inputs, declared);
        let ins = S::Params::bind_in::<K>(&mut views);
        // PANIC Safety: as above.
        assert!(views.is_empty(), "more bindings than input params");
        Self(ins)
    }
}

impl<S: Ports> SystemOutputs for OutSet<S> {
    fn defs() -> Vec<PortDef> {
        let mut outputs = Defs::of::<S>().outputs;
        outputs.push(Output::<LogEvent>::def(LOG_PORT));
        outputs
    }

    fn dynamic() -> Option<Cow<'static, str>> {
        Defs::of::<S>().dynamic_outputs
    }

    fn bind(outputs: Vec<OutputBinding>) -> Self {
        let declared = Self::defs().len();
        // PANIC Safety: the coordinator binds the declared ports, then the ones
        // it added to a dynamic bundle.
        assert!(
            outputs.len() >= declared,
            "fewer bindings than output params"
        );
        let mut writers = Bindings::new(outputs, declared);
        let outs = S::Params::bind_out(&mut writers);
        // PANIC Safety: as above, and LogEvent needs no alignment.
        let log = writers.next().expect("one writer for the log output");
        let log = LogPort::new(Output::try_new(log.writer).expect("byte alignment"));
        assert!(writers.is_empty(), "more bindings than output params");
        Self { outs, log }
    }
}
