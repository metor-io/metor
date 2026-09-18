//! The bundles a fn system binds its parameters through.

use metor_proto_wkt::LogEvent;

use crate::log::LogPort;
use crate::port::Output;
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

use super::SystemFn;
use super::param::{Bindings, Param};

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
    const DYNAMIC: bool = <S::Params as Param>::DYNAMIC_IN;

    fn defs() -> Vec<PortDef> {
        let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
        S::Params::append_defs(&mut S::NAMES.iter(), &mut inputs, &mut outputs);
        inputs
    }

    fn bind(inputs: Vec<InputBinding>) -> Self {
        let declared = Self::defs().len();
        // PANIC Safety: the coordinator binds the declared ports, then the ones
        // it added to a dynamic bundle.
        assert!(inputs.len() >= declared, "fewer bindings than input params");
        let mut views = Bindings::new(inputs, declared);
        let ins = S::Params::bind_in(&mut views);
        // PANIC Safety: as above.
        assert!(views.is_empty(), "more bindings than input params");
        Self(ins)
    }
}

impl<S: SystemFn> SystemOutputs for OutSet<S> {
    const DYNAMIC: bool = <S::Params as Param>::DYNAMIC_OUT;

    fn defs() -> Vec<PortDef> {
        let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
        S::Params::append_defs(&mut S::NAMES.iter(), &mut inputs, &mut outputs);
        outputs.push(Output::<LogEvent>::def(LOG_PORT));
        outputs
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
