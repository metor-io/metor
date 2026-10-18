//! [`InSet`] and [`OutSet`], the bound ports of a fn system.

use core::sync::atomic::AtomicU32;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::NoWake;

use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError};
use crate::io::{Io, SharedIo};
use crate::log::LogPort;
use crate::port::{InputWake, Output};
use crate::system::{InputBinding, OutputBinding, PortDef, Ports, SystemDef};

use super::FnSignature;
use super::param::{Defs, Instance, Param, Views, Writers};

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

    fn bind(inputs: Vec<InputBinding<W>>) -> Self {
        let defs = Defs::declared::<S>();
        // PANIC Safety: every make of a system with IO ports binds through
        // `fn_system::bind`.
        assert_eq!(
            defs.io_ports(),
            0,
            "IO ports bind through `fn_system::bind`"
        );
        Self::from_views(Views::new(inputs, defs.inputs.len()))
    }
}

impl<S: FnSignature> Ports<OutputBinding> for OutSet<S> {
    fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError> {
        Ok(OutSet::<S>::defs_with_log(cx)?.0)
    }

    /// Binds the outputs under an unnamed instance with seed 0 and an empty
    /// record table. A table make binds through `fn_system::bind` instead.
    fn bind(outputs: Vec<OutputBinding>) -> Self {
        let defs = Defs::declared::<S>();
        // PANIC Safety: as in `InSet::bind`.
        assert_eq!(
            defs.io_ports(),
            0,
            "IO ports bind through `fn_system::bind`"
        );
        let writers = Writers::new(outputs, defs.outputs.len() + 1, Instance::default(), None);
        // PANIC Safety: only an `Rng` parameter fails, and a table make binds
        // an `Rng` through `bind`.
        Self::from_writers(writers).expect("no Rng parameter")
    }
}

/// Returns fn system `S`'s definition, which records its `log` output.
pub(crate) fn system_def<S: FnSignature, W: InputWake>(
    cx: &DefCx<'_>,
) -> Result<SystemDef, DefError> {
    let (outputs, log) = OutSet::<S>::defs_with_log(cx)?;
    Ok(SystemDef {
        name: S::NAME.into(),
        inputs: InSet::<S, W>::defs(cx)?,
        outputs,
        log: Some(log),
        grants: Vec::new(),
    })
}

/// A fn system's bound ports.
pub(crate) type BoundSet<S, W> = (InSet<S, W>, OutSet<S>);

/// Binds a fn system's ports under `instance`, handing `io` to its `Cap`s.
pub(crate) fn bind<S: FnSignature, W: WakeSink + Clone + 'static>(
    inputs: Vec<InputBinding<W>>,
    outputs: Vec<OutputBinding>,
    instance: Instance,
    io: Option<Io>,
) -> Result<BoundSet<S, W>, ParamError> {
    let defs = Defs::declared::<S>();
    let io_ports = defs.io_ports();
    let ins = InSet::from_views(Views::new(inputs, defs.inputs.len() - io_ports));
    let writers = Writers::new(outputs, defs.outputs.len() - io_ports + 1, instance, io);
    Ok((ins, OutSet::from_writers(writers)?))
}

/// A stepped fn system's bound ports, and its queue if it has one.
pub(crate) type IoBoundSet<S> = (InSet<S>, OutSet<S>, Option<Io>);

/// Binds a stepped fn system's ports, including its queue pair.
///
/// The IO ports come after the parameters' own (see `Defs::append_io`). A
/// system with a `Cap` takes its member's arena from `shared`.
pub(crate) fn bind_with_io<S: FnSignature>(
    mut inputs: Vec<InputBinding>,
    mut outputs: Vec<OutputBinding>,
    instance: Instance,
    shared: &SharedIo,
) -> Result<IoBoundSet<S>, ParamError> {
    let defs = Defs::declared::<S>();
    let arena = defs
        .has_io()
        .then(|| shared.borrow_mut().get_or_add_member(&instance.id));
    let io_ports = defs.io_ports();
    // PANIC Safety: build binds one entry per port of the definition these
    // `Defs` describe, with `log` last among the outputs.
    let log = outputs.pop().expect("the log output");
    let io_in = inputs.split_off(inputs.len() - io_ports);
    let io_out = outputs.split_off(outputs.len() - io_ports);
    outputs.push(log);
    let io = Io::from_bindings(arena, io_in, io_out);
    let (ins, outs) = bind::<S, NoWake>(inputs, outputs, instance, io.clone())?;
    Ok((ins, outs, io))
}
