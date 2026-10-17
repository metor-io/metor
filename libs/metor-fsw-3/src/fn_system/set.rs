//! [`InSet`] and [`OutSet`], the bound ports of a fn system.

use core::sync::atomic::AtomicU32;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::NoWake;

use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError, Records};
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
        bind_inputs::<S, W>(inputs, defs.inputs.len())
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
        let writers = writers(outputs, defs.outputs.len() + 1, Arc::default());
        bind_outputs::<S>(writers)
    }

    fn bind_with_records(outputs: Vec<OutputBinding>, records: &Arc<Records>) -> Self {
        let defs = Defs::declared::<S>();
        let mut writers = writers(outputs, defs.outputs.len() + 1, Arc::default());
        writers.records = defs.records.then(|| records.clone());
        bind_outputs::<S>(writers)
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

/// A fn system's bound ports, and its queue if it has one.
pub(crate) type BoundSet<S> = (InSet<S>, OutSet<S>, Option<Io>);

/// The id, namespace, seed, and record table of the system instance being
/// bound.
#[derive(Clone, Copy)]
pub(crate) struct Instance<'a> {
    pub(crate) io: &'a SharedIo,
    pub(crate) id: &'a str,
    pub(crate) namespace: Option<&'a str>,
    /// The target seed.
    pub(crate) seed: u64,
    /// Every record the system table knows about.
    pub(crate) records: &'a Arc<Records>,
}

impl<'a> Instance<'a> {
    pub(crate) fn from_make_cx(cx: &crate::coordinator::MakeCx<'a>) -> Self {
        Self {
            io: cx.io,
            id: cx.id,
            namespace: cx.namespace,
            seed: cx.seed,
            records: cx.records,
        }
    }

    /// The stream an `Rng` parameter draws, named `{namespace}.{id}`.
    fn rng(&self) -> Rng {
        Rng::seeded(self.seed, &DefCx::path(self.namespace, self.id, ""))
    }
}

/// Binds a fn system's ports, including its queue pair.
///
/// The IO ports come after the parameters' own (see `Defs::append_io`). A
/// system with a `Cap` takes its member's arena from `io`.
pub(crate) fn bind<S: FnSignature>(
    mut inputs: Vec<InputBinding>,
    mut outputs: Vec<OutputBinding>,
    instance: Instance<'_>,
) -> Result<BoundSet<S>, ParamError> {
    let Instance { io, id, .. } = instance;
    let defs = Defs::declared::<S>();
    let rng = defs.rng.then(|| instance.rng());
    let records = defs.records.then(|| instance.records.clone());
    let arena = defs.has_io().then(|| io.borrow_mut().get_or_add_member(id));
    let io_ports = defs.io_ports();
    // PANIC Safety: build binds one entry per port of the definition these
    // `Defs` describe, with `log` last among the outputs.
    let log = outputs.pop().expect("the log output");
    let io_in = inputs.split_off(inputs.len() - io_ports);
    let io_out = outputs.split_off(outputs.len() - io_ports);
    outputs.push(log);
    let dropped = Arc::<AtomicU32>::default();
    let io = Io::from_bindings(arena, io_in, io_out);
    let ins = bind_inputs::<S, NoWake>(inputs, defs.inputs.len() - io_ports);
    let mut writers = writers(outputs, defs.outputs.len() - io_ports + 1, dropped);
    writers.io = io.clone();
    writers.rng = rng;
    writers.records = records;
    Ok((ins, bind_outputs::<S>(writers), io))
}

fn writers(outputs: Vec<OutputBinding>, declared: usize, dropped: Arc<AtomicU32>) -> Writers {
    Writers {
        bindings: Bindings::new(outputs, declared),
        dropped,
        io: None,
        rng: None,
        records: None,
    }
}

fn bind_inputs<S: FnSignature, W: InputWake>(
    inputs: Vec<InputBinding<W>>,
    outputs: Vec<OutputBinding>,
    instance: Instance,
) -> Result<BoundSet<S, W>, MakeError> {
    let defs = Defs::declared::<S>();
    let ins = InSet::from_views(Views::new(inputs, defs.inputs.len()));
    let writers = Writers::new(outputs, defs.outputs.len() + 1, instance);
    Ok((ins, OutSet::from_writers(writers)?))
}
