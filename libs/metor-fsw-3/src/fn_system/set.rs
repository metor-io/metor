//! [`InSet`] and [`OutSet`], the bound ports of a fn system.

use core::sync::atomic::{AtomicU32, Ordering};
use std::collections::VecDeque;
use std::sync::Arc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use metor_fsw_3_ring::{NoWake, WakeSink};

use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError};
use crate::io::device::DeviceSide;
use crate::io::queue::{Io, Table};
use crate::io::{Cqe, End, SharedIo, Sqe, Tables};
use crate::log::LogPort;
use crate::port::{Input, Output, Samplers};
use crate::system::{InputBinding, OutputBinding, PortDef, SystemInputs, SystemOutputs};
use std::rc::Rc;

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

impl<S: Ports> SystemOutputs for OutSet<S> {
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
}

/// A fn system's bound ports, and its queue if it has one.
pub(crate) type Bound<S> = (InSet<S>, OutSet<S>, Option<Io>);

/// Binds a fn system's ports, including its queue pair and device ends.
///
/// The IO ports come after the parameters' own (see `Defs::append_io`).
/// A system with a `Cap` takes its member's table from `io`, and each
/// `DeviceSide` resolves drivers' buffers through `io`'s tables.
pub(crate) fn bind<S: Ports>(
    mut inputs: Vec<InputBinding>,
    mut outputs: Vec<OutputBinding>,
    io: &SharedIo,
    id: &str,
) -> Result<Bound<S>, ParamError> {
    let defs = Defs::declared::<S>();
    let caps = || {
        defs.grants
            .iter()
            .filter(|g| g.end == End::Host)
            .map(|g| g.param.to_string())
    };
    let member = defs
        .io
        .then(|| io.borrow_mut().member(id, caps().collect()))
        .transpose()?;
    let table = member.map(|m| m.table.clone());
    let tables = io.borrow().tables.clone();
    let io_ports = defs.io_ports();
    // PANIC Safety: build binds one entry per port of the definition these
    // `Defs` describe, with `log` last among the outputs.
    let log = outputs.pop().expect("the log output");
    let io_in = inputs.split_off(inputs.len() - io_ports);
    let io_out = outputs.split_off(outputs.len() - io_ports);
    outputs.push(log);
    let dropped = Arc::<AtomicU32>::default();
    let (io, devices) = io_ends(table, io_in, io_out, &tables, &dropped);
    let ins = bind_inputs::<S, NoWake>(inputs, defs.inputs.len() - io_ports);
    let mut writers = writers(outputs, defs.outputs.len() - io_ports + 1, dropped);
    writers.io = io.clone();
    writers.devices = devices;
    Ok((ins, bind_outputs::<S>(writers), io))
}

/// Builds the queue pair, if any, then one device end per remaining pair of
/// IO ports.
fn io_ends(
    table: Option<Rc<Table>>,
    inputs: Vec<InputBinding>,
    outputs: Vec<OutputBinding>,
    tables: &Tables,
    dropped: &Arc<AtomicU32>,
) -> (Option<Io>, VecDeque<DeviceSide<crate::io::device::Erased>>) {
    let mut ports = inputs.into_iter().zip(outputs);
    let io = table.and_then(|table| {
        let (cq, sq) = ports.next()?;
        Some(Io::new(Output::new(sq.writer), Input::new(cq.views), table))
    });
    let devices = ports
        .map(|(req, rep)| {
            let replies = Output::<Cqe>::counting(rep.writer, dropped.clone());
            DeviceSide::new(Input::<Sqe>::new(req.views), replies, tables.clone())
        })
        .collect();
    (io, devices)
}

fn writers(outputs: Vec<OutputBinding>, declared: usize, dropped: Arc<AtomicU32>) -> Writers {
    Writers {
        bindings: Bindings::new(outputs, declared),
        dropped,
        io: None,
        devices: VecDeque::new(),
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
