//! [`InSet`] and [`OutSet`], the bound ports of a fn system.

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::{NoWake, WakeSink};

use crate::Rng;
use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError, Records};
use crate::io::{Arena, Io, SharedIo};
use crate::log::LogPort;
use crate::port::{Input, Output, Samplers};
use crate::system::{InputBinding, OutputBinding, PortDef, Ports};
use std::rc::Rc;

use super::FnSignature;
use super::param::{Bindings, Defs, Param, Views, Writers};

/// The output that every fn system writes its log events to.
pub const LOG_PORT: &str = "log";

/// The bound inputs of a fn system's parameters.
pub struct InSet<S: FnSignature, W: WakeSink + Clone + 'static = NoWake> {
    pub(crate) ins: <S::Params as Param>::In<W>,
    pub(crate) samplers: Samplers,
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

impl<S: FnSignature> OutSet<S> {
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

impl<S: FnSignature, W: WakeSink + Clone + 'static> Ports<InputBinding<W>> for InSet<S, W> {
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
        let mut outputs = Defs::of::<S>(cx)?.outputs;
        outputs.push(Output::<LogEvent>::def(cx, LOG_PORT));
        Ok(outputs)
    }

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

/// A fn system's bound ports, and its queue if it has one.
pub(crate) type BoundSet<S> = (InSet<S>, OutSet<S>, Option<Io>);

/// The id, namespace, seed, and record table of the system instance being
/// bound.
#[derive(Clone, Copy)]
pub(crate) struct Instance<'a> {
    pub(crate) io: &'a SharedIo,
    pub(crate) id: &'a str,
    pub(crate) namespace: &'a str,
    /// The target seed. A guest's make has none.
    pub(crate) seed: Option<u64>,
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
    fn rng(&self) -> Result<Rng, ParamError> {
        let system = self.id.to_string();
        let seed = self.seed.ok_or(ParamError::GuestRng { system })?;
        Ok(match self.namespace {
            "" => Rng::seeded(seed, self.id),
            ns => Rng::seeded(seed, &format!("{ns}.{}", self.id)),
        })
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
    let rng = defs.rng.then(|| instance.rng()).transpose()?;
    let records = defs.records.then(|| instance.records.clone());
    let arena = defs.io.then(|| io.borrow_mut().get_or_add_member(id));
    let io_ports = defs.io_ports();
    // PANIC Safety: build binds one entry per port of the definition these
    // `Defs` describe, with `log` last among the outputs.
    let log = outputs.pop().expect("the log output");
    let io_in = inputs.split_off(inputs.len() - io_ports);
    let io_out = outputs.split_off(outputs.len() - io_ports);
    outputs.push(log);
    let dropped = Arc::<AtomicU32>::default();
    let io = queue(arena, io_in, io_out);
    let ins = bind_inputs::<S, NoWake>(inputs, defs.inputs.len() - io_ports);
    let mut writers = writers(outputs, defs.outputs.len() - io_ports + 1, dropped);
    writers.io = io.clone();
    writers.rng = rng;
    writers.records = records;
    Ok((ins, bind_outputs::<S>(writers), io))
}

/// The queue pair over `io_cq` and `io_sq`, for a system with a `Cap`.
fn queue(
    arena: Option<Rc<Arena>>,
    inputs: Vec<InputBinding>,
    outputs: Vec<OutputBinding>,
) -> Option<Io> {
    let (cq, sq) = inputs.into_iter().zip(outputs).next()?;
    Some(Io::new(
        Output::new(sq.writer),
        Input::new(cq.views),
        arena?,
    ))
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

fn bind_inputs<S: FnSignature, W: WakeSink + Clone + 'static>(
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

fn bind_outputs<S: FnSignature>(mut writers: Writers) -> OutSet<S> {
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
