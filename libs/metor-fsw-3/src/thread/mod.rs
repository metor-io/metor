//! The adapter that runs an [`AsyncSystem`](crate::async_system::AsyncSystem)
//! on a background thread and copies its ports across.

pub(crate) mod group;
#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::sync::Arc;

use metor_fsw_3_ring::{Config, NoWake, Notifier, RingBuffer, View, WakeSource, Writer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};

use crate::coordinator::{AsyncMakeFn, ParamError, Step};
use crate::record::Record;
use crate::system::{InputBinding, OutputBinding, SystemDef};

use group::{GroupHandle, Member, Panicked};

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

/// Binds one async system: mirrors for its ports, the adapter for the cycle
/// thread, and the launch its thread will construct it from.
pub(crate) fn bind(
    make: &AsyncMakeFn,
    id: &str,
    def: &SystemDef,
    params: serde_json::Value,
    inputs: Vec<Vec<&RingBuffer>>,
    outputs: Vec<&RingBuffer>,
) -> (Thread, Member) {
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
    let member = Member {
        id: id.to_string(),
        launch: make(params, bound_in, bound_out),
        panicked: thread.panicked.clone(),
    };
    (thread, member)
}

impl Thread {
    /// Keeps the group alive for as long as this adapter is bound.
    pub(crate) fn place(&mut self, group: Arc<GroupHandle>) {
        self.group = Some(group);
    }

    /// Binds one async system on a private thread of its own, for a pack.
    pub(crate) fn alone(
        make: &AsyncMakeFn,
        id: &str,
        def: &SystemDef,
        params: serde_json::Value,
        inputs: Vec<Vec<&RingBuffer>>,
        outputs: Vec<&RingBuffer>,
    ) -> Result<Self, ParamError> {
        let (mut thread, member) = bind(make, id, def, params, inputs, outputs);
        let group =
            group::spawn(id, vec![member]).map_err(|e| ParamError::Decode(e.to_string()))?;
        thread.place(group);
        Ok(thread)
    }
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
