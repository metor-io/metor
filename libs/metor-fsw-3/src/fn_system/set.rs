//! The bundles a fn system binds its parameters through.

use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::{NoWake, WakeSink};

use crate::def::{DefCx, DefError};
use crate::log::LogPort;
use crate::port::{Output, Samplers};
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

use super::Ports;
use super::param::{Bindings, Defs, Param, Views};

/// LOG_PORT is the name of the port that receives log events.
pub const LOG_PORT: &str = "log";

/// An `InSet` is the bound inputs of a fn system's parameters, and the
/// samplers its step takes each cycle.
pub struct InSet<S: Ports, W: WakeSink + Clone + 'static = NoWake> {
    pub(crate) ins: <S::Params as Param>::In<W>,
    pub(crate) samplers: Samplers,
}

/// An `OutSet` is the bound outputs of a fn system's parameters, plus its `log`.
pub struct OutSet<S: Ports> {
    pub(crate) outs: <S::Params as Param>::Out,
    pub(crate) log: LogPort,
}

impl<S: Ports, W: WakeSink + Clone + 'static> SystemInputs<W> for InSet<S, W> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(Defs::of::<S>(cx)?.inputs)
    }

    fn bind(inputs: Vec<InputBinding<W>>) -> Self {
        let declared = Defs::declared::<S>().inputs.len();
        let mut views = Views {
            bindings: Bindings::new(inputs, declared),
            samplers: Samplers::default(),
        };
        let ins = S::Params::bind_in::<W>(&mut views);
        // PANIC Safety: the coordinator binds one list per port of a
        // definition these parameters computed.
        assert!(views.bindings.is_empty(), "more bindings than input params");
        Self {
            ins,
            samplers: views.samplers,
        }
    }
}

impl<S: Ports> SystemOutputs for OutSet<S> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        let mut outputs = Defs::of::<S>(cx)?.outputs;
        outputs.push(Output::<LogEvent>::def(cx, LOG_PORT));
        Ok(outputs)
    }

    fn bind(outputs: Vec<OutputBinding>) -> Self {
        let declared = Defs::declared::<S>().outputs.len();
        let mut writers = Bindings::new(outputs, declared + 1);
        let outs = S::Params::bind_out(&mut writers);
        // PANIC Safety: the log is the last port of every `defs`.
        let log = writers.next().expect("one writer for the log output");
        let log = LogPort::new(Output::new(log.writer));
        assert!(writers.is_empty(), "more bindings than output params");
        Self { outs, log }
    }
}
