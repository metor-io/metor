//! [`InSet`] and [`OutSet`], the bound ports of a fn system.

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::{NoWake, WakeSink};

use crate::Rng;
use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError};
use crate::log::LogPort;
use crate::port::{Output, Samplers};
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};

use super::Ports;
use super::param::{Bindings, Defs, Param, Views, Writers};

/// The output that every fn system writes its log events to.
pub const LOG_PORT: &str = "log";

/// The bound inputs of a fn system's parameters.
pub struct InSet<S: Ports, W: WakeSink + Clone + 'static = NoWake> {
    pub(crate) ins: <S::Params as Param>::In<W>,
    pub(crate) samplers: Samplers,
}

/// The bound outputs of a fn system's parameters, plus its `log` output.
///
/// Failed writes on any of the outputs are counted and reported together on
/// `log`.
pub struct OutSet<S: Ports> {
    pub(crate) outs: <S::Params as Param>::Out,
    pub(crate) log: LogPort,
    pub(crate) dropped: Arc<AtomicU32>,
}

impl<S: Ports> OutSet<S> {
    /// Reports the writes that failed since the last report.
    ///
    /// Writes at most one fault on `log`.
    pub(crate) fn report_dropped(&mut self, now: Timestamp) {
        report_dropped(&mut self.log, &self.dropped, now);
    }
}

/// Reports and resets the count in `dropped`.
///
/// If the count was nonzero, writes one `publish_dropped` fault on `log`.
pub(crate) fn report_dropped(log: &mut LogPort, dropped: &AtomicU32, now: Timestamp) {
    let count = dropped.swap(0, Ordering::Relaxed);
    if count != 0 {
        log.fault(now, "publish_dropped", format!("{count} writes dropped"));
    }
}

impl<S: Ports, W: WakeSink + Clone + 'static> SystemInputs<W> for InSet<S, W> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(Defs::of::<S>(cx)?.inputs)
    }

    fn bind(inputs: Vec<InputBinding<W>>) -> Self {
        let defs = Defs::declared::<S>();
        bind_inputs::<S, W>(inputs, defs.inputs.len())
    }
}

impl<S: Ports> SystemOutputs for OutSet<S> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        let mut outputs = Defs::of::<S>(cx)?.outputs;
        outputs.push(Output::<LogEvent>::def(cx, LOG_PORT));
        Ok(outputs)
    }

    fn bind(outputs: Vec<OutputBinding>) -> Self {
        let defs = Defs::declared::<S>();
        let writers = writers(outputs, defs.outputs.len() + 1, Arc::default());
        bind_outputs::<S>(writers)
    }
}

/// A fn system's bound ports.
pub(crate) type Bound<S> = (InSet<S>, OutSet<S>);

/// The system instance a fn system binds for.
#[derive(Clone, Copy)]
pub(crate) struct Instance<'a> {
    pub(crate) id: &'a str,
    pub(crate) namespace: &'a str,
    /// The target seed. A guest's make has none.
    pub(crate) seed: Option<u64>,
}

impl<'a> Instance<'a> {
    pub(crate) fn of(cx: &crate::coordinator::MakeCx<'a>) -> Self {
        Self {
            id: cx.id,
            namespace: cx.namespace,
            seed: cx.seed,
        }
    }

    /// The stream an `Rng` parameter draws, named `{namespace}.{id}`.
    fn rng(&self) -> Result<Rng, ParamError> {
        let system = self.id.to_string();
        let seed = self.seed.ok_or(ParamError::GuestRng { system })?;
        Ok(match self.namespace {
            "" => Rng::seeded(seed, self.id),
            ns => Rng::seeded(seed, &format!("{ns}.{}", self.id)),
        })
    }
}

/// Binds a fn system's ports.
pub(crate) fn bind<S: Ports>(
    inputs: Vec<InputBinding>,
    outputs: Vec<OutputBinding>,
    instance: Instance<'_>,
) -> Result<Bound<S>, ParamError> {
    let defs = Defs::declared::<S>();
    let rng = defs.rng.then(|| instance.rng()).transpose()?;
    let dropped = Arc::<AtomicU32>::default();
    let ins = bind_inputs::<S, NoWake>(inputs, defs.inputs.len());
    let mut writers = writers(outputs, defs.outputs.len() + 1, dropped);
    writers.rng = rng;
    Ok((ins, bind_outputs::<S>(writers)))
}

fn writers(outputs: Vec<OutputBinding>, declared: usize, dropped: Arc<AtomicU32>) -> Writers {
    Writers {
        bindings: Bindings::new(outputs, declared),
        dropped,
        rng: None,
    }
}

fn bind_inputs<S: Ports, W: WakeSink + Clone + 'static>(
    inputs: Vec<InputBinding<W>>,
    declared: usize,
) -> InSet<S, W> {
    let mut views = Views {
        bindings: Bindings::new(inputs, declared),
        samplers: Samplers::default(),
    };
    let ins = S::Params::bind_in::<W>(&mut views);
    // PANIC Safety: the coordinator binds one list per port of the
    // definition these parameters produced.
    assert!(views.bindings.is_empty(), "more bindings than input params");
    InSet {
        ins,
        samplers: views.samplers,
    }
}

fn bind_outputs<S: Ports>(mut writers: Writers) -> OutSet<S> {
    let outs = S::Params::bind_out(&mut writers);
    // PANIC Safety: `defs` always appends the log as the last output.
    let log = writers
        .bindings
        .next()
        .expect("one writer for the log output");
    let log = LogPort::new(Output::new(log.writer));
    assert!(
        writers.bindings.is_empty(),
        "more bindings than output params"
    );
    OutSet {
        outs,
        log,
        dropped: writers.dropped,
    }
}
