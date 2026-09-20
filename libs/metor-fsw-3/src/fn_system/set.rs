//! The bundles a fn system binds its parameters through.

use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::{NoWake, WakeSink};

use crate::coordinator::OutputConfig;
use crate::def::Records;
use crate::def::{DefCx, DefError};
use crate::log::LogPort;
use crate::port::Output;
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

use super::Ports;
use super::param::{Bindings, Defs, Param};

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
        Ok(Defs::of::<S>(cx)?.inputs)
    }

    fn bind(inputs: Vec<InputBinding<W>>) -> Self {
        let counts = input_counts::<S, W>(&inputs);
        let mut views = Bindings::new(inputs, counts);
        let ins = S::Params::bind_in::<W>(&mut views);
        // PANIC Safety: the coordinator binds one list per port of the
        // definition these counts were taken from.
        assert!(views.is_empty(), "more bindings than input params");
        Self(ins)
    }
}

impl<S: Ports> SystemOutputs for OutSet<S> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        let mut outputs = Defs::of::<S>(cx)?.outputs;
        outputs.push(Output::<LogEvent>::def(LOG_PORT));
        Ok(outputs)
    }

    fn bind(outputs: Vec<OutputBinding>) -> Self {
        let counts = output_counts::<S>(&outputs);
        let mut writers = Bindings::new(outputs, counts);
        let outs = S::Params::bind_out(&mut writers);
        // PANIC Safety: the log is the last port of every `defs`, and
        // LogEvent needs no alignment.
        let log = writers.next().expect("one writer for the log output");
        let log = LogPort::new(Output::try_new(log.writer).expect("byte alignment"));
        assert!(writers.is_empty(), "more bindings than output params");
        Self { outs, log }
    }
}

/// The share of the bindings each input parameter takes, recovered by walking
/// the parameters over the ports they were bound with.
fn input_counts<S: Ports, W: WakeSink + Clone + 'static>(
    bindings: &[InputBinding<W>],
) -> Vec<usize> {
    let ports: Vec<(&str, &PortDef)> = bindings
        .iter()
        .map(|binding| (binding.def.name.as_ref(), &binding.def))
        .collect();
    // PANIC Safety: these ports are a definition the parameters computed.
    Defs::of::<S>(&DefCx::of_inputs(&ports))
        .expect("a bound definition")
        .input_counts
}

/// As [`input_counts`], over every port but the trailing `log`.
fn output_counts<S: Ports>(bindings: &[OutputBinding]) -> Vec<usize> {
    let declared = &bindings[..bindings.len().saturating_sub(1)];
    let outputs: Vec<OutputConfig> = declared
        .iter()
        .map(|binding| OutputConfig {
            port: binding.def.name.to_string(),
            record: binding.def.record.to_string(),
        })
        .collect();
    let records = Records::of(declared.iter().map(|binding| &binding.def));
    // PANIC Safety: as for `input_counts`.
    let mut counts = Defs::of::<S>(&DefCx::of_outputs(&outputs, &records))
        .expect("a bound definition")
        .output_counts;
    counts.push(1);
    counts
}
