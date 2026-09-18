//! The adapter that runs an [`AsyncSystem`](crate::async_system::AsyncSystem)
//! on a background thread and copies its ports across.

pub(crate) mod group;
#[cfg(test)]
mod tests;
mod threads;

use std::borrow::Cow;
use std::marker::PhantomData;
use std::sync::Arc;

use metor_fsw_3_ring::{Config, NoWake, Notifier, RingBuffer, View, WakeSource, Writer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};

use crate::async_system::{AsyncSystem, Running, Stop};
use crate::coordinator::{MakeCx, ParamError, Params, Step};
use crate::record::Record;
use crate::system::{InputBinding, OutputBinding, SystemDef, SystemInputs, SystemOutputs};

use group::{GroupHandle, Member, Panicked};

pub use threads::Threads;

/// The thread every async system lands on unless its config names another.
pub const DEFAULT_THREAD: &str = "default";

/// A `Launch` constructs one async system on the thread that will run it.
pub(crate) trait Launch: Send {
    fn launch(self: Box<Self>, stop: Stop) -> Result<Running, ParamError>;
}

/// One async system's ingredients, owned until its thread constructs it.
struct AsyncLaunch<A: AsyncSystem, M> {
    make: Arc<M>,
    params: serde_json::Value,
    inputs: Vec<InputBinding<Notifier>>,
    outputs: Vec<OutputBinding>,
    _a: PhantomData<fn() -> A>,
}

impl<A, M> Launch for AsyncLaunch<A, M>
where
    A: AsyncSystem + 'static,
    M: Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
{
    fn launch(self: Box<Self>, stop: Stop) -> Result<Running, ParamError> {
        let (system, mut state) = (self.make)(Params(&self.params))?;
        let mut inputs = A::Inputs::bind(self.inputs);
        let mut outputs = A::Outputs::bind(self.outputs);
        Ok(Box::pin(async move {
            system
                .run(&mut state, &mut inputs, &mut outputs, stop)
                .await;
        }))
    }
}

/// Places one async system on the group `cx` names: mirrors for its ports, the
/// system built and running on that group's thread, and the adapter that
/// copies between them each cycle.
pub(crate) fn place<A, M>(
    make: Arc<M>,
    params: serde_json::Value,
    def: &SystemDef,
    inputs: Vec<Vec<&RingBuffer>>,
    outputs: Vec<&RingBuffer>,
    cx: &mut MakeCx<'_>,
) -> Result<Box<dyn Step>, ParamError>
where
    A: AsyncSystem + 'static,
    M: Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
{
    let (mut thread, bound_in, bound_out) = bind(def, inputs, outputs);
    let member = Member {
        launch: Box::new(AsyncLaunch::<A, M> {
            make,
            params,
            inputs: bound_in,
            outputs: bound_out,
            _a: PhantomData,
        }),
        panicked: thread.panicked.clone(),
    };
    let group = cx
        .threads
        .get_or_spawn(cx.thread)
        .map_err(|e| ParamError::Decode(e.to_string()))?;
    group.add(member)?;
    thread.group = Some(group);
    Ok(Box::new(thread))
}

/// How much deeper a mirror ring is than the ring it mirrors.
const MIRROR_FACTOR: usize = 4;

/// The port name the adapter writes its own lines on.
const LOG_PORT: &str = "log";

/// One ring copied into another, and the records the destination refused.
struct Mirror<W: WakeSource> {
    port: Cow<'static, str>,
    from: View<NoWake>,
    into: Writer<W>,
    dropped: u64,
}

impl<W: WakeSource> Mirror<W> {
    /// Moves every waiting record, counting the ones that found no room.
    fn drain(&mut self, scratch: &mut Vec<u8>) {
        while matches!(self.from.try_read_into(scratch), Ok(true)) {
            if self.into.try_write(scratch).is_err() {
                self.dropped += 1;
            }
        }
    }
}

/// A `Thread` is the step an async system is bound as.
///
/// Each cycle it drains the real input rings into the system's mirrors and the
/// system's output mirrors into the real rings. Both are byte copies; a mirror
/// with no room drops the record for this system alone.
pub struct Thread {
    /// The real input rings into the mirrors the system reads, whose writers
    /// wake it, and the system's output mirrors into the real rings.
    inputs: Vec<Mirror<Notifier>>,
    outputs: Vec<Mirror<NoWake>>,
    /// Which output is the system's `log`, where the adapter's lines go too.
    log: Option<usize>,
    scratch: Vec<u8>,
    line: Vec<u8>,
    panicked: Panicked,
    latched: bool,
    group: Option<Arc<GroupHandle>>,
}

impl Step for Thread {
    fn execute(&mut self, now: Timestamp) {
        for mirror in self.inputs.iter_mut() {
            mirror.drain(&mut self.scratch);
        }
        for mirror in self.outputs.iter_mut() {
            mirror.drain(&mut self.scratch);
        }
        self.report_drops(now);
        self.check_panic(now);
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.write_line(now, LogLevel::Error, "panic", message);
    }

    fn latched(&self) -> bool {
        self.latched
    }
}

impl Thread {
    /// One line naming every port that dropped a record this cycle.
    fn report_drops(&mut self, now: Timestamp) {
        let dropped = self.inputs.iter().any(|mirror| mirror.dropped > 0)
            || self.outputs.iter().any(|mirror| mirror.dropped > 0);
        if !dropped {
            return;
        }
        let fields = self.drop_fields();
        self.emit(LogEvent {
            timestamp: now,
            level: LogLevel::Error,
            source: Cow::Borrowed(""),
            target: Cow::Borrowed(""),
            message: Cow::Borrowed("records dropped into a full mirror"),
            span: None,
            fields,
            file: None,
            line: None,
        });
    }

    /// The per-port counts, cleared as they are read.
    fn drop_fields(&mut self) -> Vec<(Cow<'static, str>, Cow<'static, str>)> {
        let mut fields = vec![(Cow::Borrowed("kind"), Cow::Borrowed("mirror_dropped"))];
        for mirror in self.inputs.iter_mut() {
            if mirror.dropped > 0 {
                fields.push((mirror.port.clone(), mirror.dropped.to_string().into()));
                mirror.dropped = 0;
            }
        }
        for mirror in self.outputs.iter_mut() {
            if mirror.dropped > 0 {
                fields.push((mirror.port.clone(), mirror.dropped.to_string().into()));
                mirror.dropped = 0;
            }
        }
        fields
    }

    /// Latches the system off once its task has panicked, reporting it once.
    fn check_panic(&mut self, now: Timestamp) {
        if self.latched {
            return;
        }
        // PANIC Safety: the task only replaces this slot, never panicking while
        // it holds the lock.
        let message = self.panicked.lock().expect("an unpoisoned slot").take();
        if let Some(message) = message {
            self.latched = true;
            self.write_line(now, LogLevel::Error, "panic", &message);
        }
    }

    fn write_line(&mut self, now: Timestamp, level: LogLevel, kind: &'static str, message: &str) {
        self.emit(LogEvent {
            timestamp: now,
            level,
            source: Cow::Borrowed(""),
            target: Cow::Borrowed(""),
            message: message.to_string().into(),
            span: None,
            fields: vec![(Cow::Borrowed("kind"), Cow::Borrowed(kind))],
            file: None,
            line: None,
        });
    }

    /// Publishes one line on the system's own `log`, if it declares one.
    fn emit(&mut self, event: LogEvent) {
        let Some(log) = self.log else { return };
        let Ok(bytes) = event.encode(&mut self.line) else {
            return;
        };
        let _ = self.outputs[log].into.try_write(bytes);
    }
}

/// Mirrors every port of one async system, returning the adapter for the cycle
/// thread and the bindings its own thread reads and writes.
fn bind(
    def: &SystemDef,
    inputs: Vec<Vec<&RingBuffer>>,
    outputs: Vec<&RingBuffer>,
) -> (Thread, Vec<InputBinding<Notifier>>, Vec<OutputBinding>) {
    let wake = Notifier::default();
    let mut edges = Vec::new();
    let mut bound_in = Vec::with_capacity(def.inputs.len());
    for (port, rings) in def.inputs.iter().zip(inputs) {
        let mut views = Vec::with_capacity(rings.len());
        for ring in rings {
            let mirror = mirror(ring);
            // PANIC Safety: a fresh mirror has its writer and one reader slot free.
            edges.push(Mirror {
                port: port.name.clone(),
                from: ring.view(NoWake).expect("a counted reader slot"),
                into: mirror.writer(wake.clone()).expect("a fresh mirror"),
                dropped: 0,
            });
            views.push(mirror.view(wake.clone()).expect("a fresh mirror"));
        }
        bound_in.push(InputBinding {
            def: port.clone(),
            views,
        });
    }

    let mut copies = Vec::with_capacity(def.outputs.len());
    let mut bound_out = Vec::with_capacity(def.outputs.len());
    for (port, ring) in def.outputs.iter().zip(outputs) {
        let mirror = mirror(ring);
        copies.push(Mirror {
            port: port.name.clone(),
            // PANIC Safety: as above; the real output ring has one writer.
            from: mirror.view(NoWake).expect("a fresh mirror"),
            into: ring.writer(NoWake).expect("one writer per output ring"),
            dropped: 0,
        });
        bound_out.push(OutputBinding {
            def: port.clone(),
            writer: mirror.writer(NoWake).expect("a fresh mirror"),
        });
    }

    let thread = Thread {
        scratch: Vec::with_capacity(scratch_len(def)),
        line: vec![0; LogEvent::MAX_LEN],
        log: log_port(def),
        inputs: edges,
        outputs: copies,
        panicked: Panicked::default(),
        latched: false,
        group: None,
    };
    (thread, bound_in, bound_out)
}

/// A ring of the same records as `real`, several times as deep.
fn mirror(real: &RingBuffer) -> RingBuffer {
    let config = real.config();
    RingBuffer::create_in_memory(Config {
        capacity: config.capacity * MIRROR_FACTOR,
        max_readers: 1,
    })
}

/// The largest record any of this system's ports carries.
fn scratch_len(def: &SystemDef) -> usize {
    def.inputs
        .iter()
        .chain(&def.outputs)
        .map(|port| port.max_len)
        .max()
        .unwrap_or(0)
}

/// The index of the system's `log` output, which is where its lines go.
fn log_port(def: &SystemDef) -> Option<usize> {
    def.outputs
        .iter()
        .position(|port| port.name == LOG_PORT && port.id == LogEvent::ID)
}
