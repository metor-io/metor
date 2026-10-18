//! [`InSet`] and [`OutSet`], the bound ports of a fn system.

use core::sync::atomic::AtomicU32;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::NoWake;

use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError};
use crate::log::LogPort;
use crate::port::{InputWake, Output};
use crate::system::{BindCx, InputBinding, OutputBinding, PortDef, Ports, SystemDef};

use super::FnSignature;
use super::param::{Defs, Param, Views, Writers};

/// The output that every fn system writes its log events to.
pub const LOG_PORT: &str = "log";

/// The bound inputs of a fn system's parameters.
pub struct InSet<S: FnSignature, W: InputWake = NoWake> {
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

impl<S: FnSignature, W: InputWake> InSet<S, W> {
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
    /// The parameters' output defs with `log` appended, and the index of
    /// `log`.
    fn defs_with_log(cx: &DefCx<'_>) -> Result<(Vec<PortDef>, usize), DefError> {
        let mut outputs = Defs::of::<S>(cx)?.outputs;
        let log = outputs.len();
        outputs.push(Output::<LogEvent>::def(cx, LOG_PORT));
        Ok((outputs, log))
    }

    /// Hands `writers` to the parameters, in order, and the last one to the
    /// log.
    ///
    /// # Errors
    /// Fails if a parameter's output does not bind.
    pub(crate) fn from_writers(mut writers: Writers) -> Result<Self, MakeError> {
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

impl<S: FnSignature, W: InputWake> Ports<InputBinding<W>> for InSet<S, W> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(Defs::of::<S>(cx)?.inputs)
    }

    fn bind(inputs: Vec<InputBinding<W>>, _cx: &BindCx) -> Result<Self, MakeError> {
        let defs = Defs::declared::<S>();
        Ok(Self::from_views(Views::new(inputs, defs.param_inputs())))
    }
}

impl<S: FnSignature> Ports<OutputBinding> for OutSet<S> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(OutSet::<S>::defs_with_log(cx)?.0)
    }

    /// Binds the outputs, then `log`, handing `cx`'s queue to the `Cap`s.
    fn bind(outputs: Vec<OutputBinding>, cx: &BindCx) -> Result<Self, MakeError> {
        let defs = Defs::declared::<S>();
        let declared = defs.param_outputs() + 1;
        Self::from_writers(Writers::new(
            outputs,
            declared,
            cx.instance.clone(),
            cx.io.clone(),
        ))
    }
}

/// Returns fn system `S`'s definition, which records its `log` output and
/// its queue pair's ports.
pub(crate) fn system_def<S: FnSignature>(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let defs = Defs::of::<S>(cx)?;
    let (outputs, log) = OutSet::<S>::defs_with_log(cx)?;
    Ok(SystemDef {
        name: S::NAME.into(),
        inputs: defs.inputs,
        outputs,
        log: Some(log),
        grants: defs.grants,
        io: defs.io,
    })
}
