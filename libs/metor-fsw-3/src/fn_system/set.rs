//! [`InSet`] and [`OutSet`], the bound ports of a fn system.

use core::sync::atomic::AtomicU32;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::{NoWake, WakeSink};

use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError};
use crate::log::LogPort;
use crate::port::Output;
use crate::system::{InputBinding, OutputBinding, PortDef, Ports};

use super::FnSignature;
use super::param::{Defs, Instance, Param, Views, Writers};

/// The output that every fn system writes its log events to.
pub const LOG_PORT: &str = "log";

/// The bound inputs of a fn system's parameters.
pub struct InSet<S: FnSignature, W: WakeSink + Clone + 'static = NoWake> {
    pub(crate) ins: <S::Params as Param>::In<W>,
}

/// The bound outputs of a fn system's parameters, plus its `log` output.
///
/// Failed writes on any of the outputs are counted and reported together on
/// `log`.
pub struct OutSet<S: FnSignature> {
    pub(crate) outs: <S::Params as Param>::Out,
    pub(crate) log: LogPort,
    pub(crate) dropped: Arc<AtomicU32>,
}

impl<S: FnSignature, W: WakeSink + Clone + 'static> InSet<S, W> {
    /// Hands `views` to the parameters, in order.
    pub(crate) fn from_views(mut views: Views<W>) -> Self {
        let ins = S::Params::bind_in::<W>(&mut views);
        // PANIC Safety: the coordinator binds one list per port of the
        // definition these parameters produced.
        assert!(views.bindings.is_empty(), "more bindings than input params");
        Self { ins }
    }
}

impl<S: FnSignature> OutSet<S> {
    /// Hands `writers` to the parameters, in order, and the last one to the
    /// log.
    ///
    /// # Errors
    /// Fails if an `Rng` parameter's instance has no seed.
    pub(crate) fn from_writers(mut writers: Writers) -> Result<Self, ParamError> {
        let outs = S::Params::bind_out(&mut S::NAMES.iter(), &mut writers)?;
        // PANIC Safety: `defs` always appends the log as the last output.
        let log = writers
            .bindings
            .next()
            .expect("one writer for the log output");
        assert!(
            writers.bindings.is_empty(),
            "more bindings than output params"
        );
        Ok(Self {
            outs,
            log: LogPort::new(Output::new(log.writer)),
            dropped: writers.dropped,
        })
    }

    /// Reports the writes that failed since the last report.
    ///
    /// Writes at most one fault on `log`.
    pub(crate) fn report_failed_writes(&mut self, now: Timestamp) {
        self.log.report_failed_writes(&self.dropped, now);
    }
}

impl<S: FnSignature, W: WakeSink + Clone + 'static> Ports<InputBinding<W>> for InSet<S, W> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(Defs::of::<S>(cx)?.inputs)
    }

    fn bind(inputs: Vec<InputBinding<W>>) -> Self {
        let defs = Defs::declared::<S>();
        Self::from_views(Views::new(inputs, defs.inputs.len()))
    }
}

impl<S: FnSignature> Ports<OutputBinding> for OutSet<S> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        let mut outputs = Defs::of::<S>(cx)?.outputs;
        outputs.push(Output::<LogEvent>::def(cx, LOG_PORT));
        Ok(outputs)
    }

    /// Binds the outputs under an unnamed instance with no seed and an empty
    /// record table. A table make binds through `fn_system::bind` instead.
    fn bind(outputs: Vec<OutputBinding>) -> Self {
        let defs = Defs::declared::<S>();
        let writers = Writers::new(outputs, defs.outputs.len() + 1, Instance::default());
        // PANIC Safety: only an `Rng` parameter fails, and a table make binds
        // an `Rng` through `bind`.
        Self::from_writers(writers).expect("no Rng parameter")
    }
}

/// A fn system's bound ports.
pub(crate) type BoundSet<S, W> = (InSet<S, W>, OutSet<S>);

/// Binds a fn system's ports under `instance`.
pub(crate) fn bind<S: FnSignature, W: WakeSink + Clone + 'static>(
    inputs: Vec<InputBinding<W>>,
    outputs: Vec<OutputBinding>,
    instance: Instance,
) -> Result<BoundSet<S, W>, ParamError> {
    let defs = Defs::declared::<S>();
    let ins = InSet::from_views(Views::new(inputs, defs.inputs.len()));
    let writers = Writers::new(outputs, defs.outputs.len() + 1, instance);
    Ok((ins, OutSet::from_writers(writers)?))
}
