//! The bundles a fn system binds its parameters through.

use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::{NoWake, WakeSink};

use crate::def::{DefCx, DefError};
use crate::log::LogPort;
use crate::port::Output;
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

use std::borrow::Cow;

use super::Ports;
use super::param::{Bindings, Param, PortDefs};

/// The ports a system's parameters declare whatever its config says.
fn static_defs<S: Ports>() -> PortDefs {
    // PANIC Safety: no parameter refuses the empty context.
    PortDefs::of::<S>(&DefCx::empty()).expect("a static definition")
}

/// LOG_PORT is the name of the port that receives log events.
pub const LOG_PORT: &str = "log";

/// An `InSet` is the bound inputs of a fn system's parameters.
pub struct InSet<S: Ports, W: WakeSink + Clone + 'static = NoWake>(
    pub(crate) <S::Params as Param>::In<W>,
);

/// An `OutSet` is the bound outputs of a fn system's parameters, plus its `log`.
pub struct OutSet<S: Ports> {
    pub(crate) outs: <S::Params as Param>::Out,
    pub(crate) log: LogPort,
}

impl<S: Ports, W: WakeSink + Clone + 'static> SystemInputs<W> for InSet<S, W> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(PortDefs::of::<S>(cx)?.inputs)
    }

    fn dynamic() -> Option<Cow<'static, str>> {
        static_defs::<S>().dynamic_inputs
    }

    fn bind(inputs: Vec<InputBinding<W>>) -> Self {
        let declared = static_defs::<S>().inputs.len();
        // PANIC Safety: the coordinator binds the declared ports, then the ones
        // it added to a dynamic bundle.
        assert!(inputs.len() >= declared, "fewer bindings than input params");
        let mut views = Bindings::new(inputs, declared);
        let ins = S::Params::bind_in::<W>(&mut views);
        // PANIC Safety: as above.
        assert!(views.is_empty(), "more bindings than input params");
        Self(ins)
    }
}

impl<S: Ports> SystemOutputs for OutSet<S> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        let mut outputs = PortDefs::of::<S>(cx)?.outputs;
        outputs.push(Output::<LogEvent>::def(LOG_PORT));
        Ok(outputs)
    }

    fn dynamic() -> Option<Cow<'static, str>> {
        static_defs::<S>().dynamic_outputs
    }

    fn bind(outputs: Vec<OutputBinding>) -> Self {
        let declared = static_defs::<S>().outputs.len() + 1;
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
