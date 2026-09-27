//! Copies of a system's rings across an adapter boundary, and the log lines
//! that report what the copies dropped.

use core::fmt::Write;
use std::borrow::Cow;

use metor_fsw_3_ring::{Config, NoWake, ReadError, View, WakeSource, WriteError, Writer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};

use crate::coordinator::ParamError;
use crate::fn_system::LOG_PORT;
use crate::record::Record;
use crate::system::SystemDef;

/// Cycles between two drop reports; the counts accumulate between them.
const DROP_REPORT_CYCLES: u64 = 100;

/// A log line's fields, as the mirrors keep them between cycles.
type Fields = Vec<(Cow<'static, str>, Cow<'static, str>)>;

/// A ring on either side of a mirror broke a structural invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Corrupt;

impl From<ReadError> for Corrupt {
    fn from(ReadError::Corrupt: ReadError) -> Self {
        Corrupt
    }
}

/// One ring copied into another, and the records the destination refused.
pub(crate) struct Mirror<W: WakeSource> {
    pub(crate) port: Cow<'static, str>,
    pub(crate) from: View<NoWake>,
    pub(crate) into: Writer<W>,
    pub(crate) dropped: u64,
}

impl<W: WakeSource> Mirror<W> {
    /// Moves every waiting record, counting the ones that found no room.
    /// Stops at the first corrupt record or destination, keeping the records
    /// before it moved.
    pub(crate) fn drain(&mut self) -> Result<(), Corrupt> {
        let moved = self
            .from
            .drain()
            .try_for_each(|record| match self.into.try_write(record?) {
                Ok(()) => Ok(()),
                Err(WriteError::Corrupt) => Err(Corrupt),
                Err(WriteError::WouldBlock | WriteError::InsufficientCapacity) => {
                    self.dropped += 1;
                    Ok(())
                }
            });
        self.from.settle();
        moved
    }
}

/// `Mirrors` are every port of one system, copied across a boundary: the real
/// input rings into the system's copies, and its copies into the real outputs.
pub(crate) struct Mirrors<W: WakeSource> {
    inputs: Vec<Mirror<W>>,
    outputs: Vec<Mirror<NoWake>>,
    /// Which output is the system's `log`, where the adapter's lines go too.
    log: Option<usize>,
    line: Vec<u8>,
    /// One field per port, written in place, so a drop report allocates nothing.
    fields: Fields,
    since_report: u64,
}

impl<W: WakeSource> Mirrors<W> {
    /// Mirrors over `def`'s ports, in bind order.
    pub(crate) fn new(
        def: &SystemDef,
        inputs: Vec<Mirror<W>>,
        outputs: Vec<Mirror<NoWake>>,
    ) -> Self {
        let ports = inputs
            .iter()
            .map(|mirror| &mirror.port)
            .chain(outputs.iter().map(|mirror| &mirror.port));
        Self {
            line: vec![0; LogEvent::MAX_LEN],
            fields: drop_fields(ports),
            since_report: 0,
            log: log_port(def),
            inputs,
            outputs,
        }
    }

    /// Drains every input mirror, returning the first corruption.
    pub(crate) fn drain_inputs(&mut self) -> Result<(), Corrupt> {
        self.inputs
            .iter_mut()
            .map(Mirror::drain)
            .fold(Ok(()), Result::and)
    }

    /// Drains every output mirror, returning the first corruption.
    pub(crate) fn drain_outputs(&mut self) -> Result<(), Corrupt> {
        self.outputs
            .iter_mut()
            .map(Mirror::drain)
            .fold(Ok(()), Result::and)
    }

    /// Drops the input mirrors, freeing their reader slots on the real rings.
    pub(crate) fn release_inputs(&mut self) {
        self.fields.drain(1..=self.inputs.len());
        self.inputs.clear();
    }

    /// One line per [`DROP_REPORT_CYCLES`], counting what every port dropped
    /// since the last one.
    pub(crate) fn report_drops(&mut self, now: Timestamp) {
        self.since_report += 1;
        if self.since_report < DROP_REPORT_CYCLES {
            return;
        }
        self.since_report = 0;
        if !self.take_drops() {
            return;
        }
        let fields = core::mem::take(&mut self.fields);
        let event = crate::log::line(
            now,
            LogLevel::Error,
            "records dropped into a full mirror",
            fields,
        );
        self.emit(&event);
        self.fields = event.fields;
    }

    /// Publishes one line with a `kind` field on the system's own `log`.
    pub(crate) fn write_line(
        &mut self,
        now: Timestamp,
        level: LogLevel,
        kind: &'static str,
        message: &str,
    ) {
        let fields = vec![(Cow::Borrowed("kind"), Cow::Borrowed(kind))];
        self.emit(&crate::log::line(now, level, message.to_string(), fields));
    }

    /// Writes each port's count into its own field, clearing the counts, and
    /// reports whether any port dropped a record.
    fn take_drops(&mut self) -> bool {
        let mut dropped = false;
        let counts = counts(&mut self.inputs, &mut self.outputs);
        for ((_, field), count) in self.fields[1..].iter_mut().zip(counts) {
            dropped |= count > 0;
            set_count(field, count);
        }
        dropped
    }

    /// Publishes one line on the system's own `log`, if it declares one.
    fn emit(&mut self, event: &LogEvent) {
        let Some(log) = self.log else { return };
        let Ok(bytes) = event.encode(&mut self.line) else {
            return;
        };
        let _ = self.outputs[log].into.try_write(bytes);
    }
}

/// Each mirror's drop count, inputs then outputs, cleared as it is read.
fn counts<'a, W: WakeSource>(
    inputs: &'a mut [Mirror<W>],
    outputs: &'a mut [Mirror<NoWake>],
) -> impl Iterator<Item = u64> + 'a {
    let inputs = inputs.iter_mut().map(|m| core::mem::take(&mut m.dropped));
    let outputs = outputs.iter_mut().map(|m| core::mem::take(&mut m.dropped));
    inputs.chain(outputs)
}

/// Writes `count` into a field's own buffer, which allocates nothing.
fn set_count(value: &mut Cow<'static, str>, count: u64) {
    let Cow::Owned(text) = value else { return };
    text.clear();
    // PANIC Safety: writing into a `String` cannot fail.
    let _ = write!(text, "{count}");
}

/// One field per port, holding the count it dropped, in bind order.
fn drop_fields<'a>(ports: impl Iterator<Item = &'a Cow<'static, str>>) -> Fields {
    let kind = (Cow::Borrowed("kind"), Cow::Borrowed("mirror_dropped"));
    let counts = ports.map(|port| {
        let count = Cow::Owned(String::with_capacity(20));
        (port.clone(), count)
    });
    core::iter::once(kind).chain(counts).collect()
}

/// The geometry of a mirror `factor` times as deep as a ring of `capacity`
/// bytes, with one reader.
pub(crate) fn mirror_config(capacity: usize, factor: usize) -> Result<Config, ParamError> {
    let too_large = ParamError::MirrorTooLarge { capacity };
    let config = Config {
        capacity: capacity.checked_mul(factor).ok_or(too_large.clone())?,
        max_readers: 1,
    };
    metor_fsw_3_ring::checked_region_len(&config).ok_or(too_large)?;
    Ok(config)
}

/// The index of the system's `log` output, which is where its lines go.
fn log_port(def: &SystemDef) -> Option<usize> {
    def.outputs
        .iter()
        .position(|port| port.name == LOG_PORT && port.id == LogEvent::ID)
}

#[cfg(test)]
pub(crate) mod tests {
    use metor_fsw_3_ring::RingBuffer;

    use super::*;

    fn ring(capacity: usize) -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity,
            max_readers: 1,
        })
    }

    fn mirror(from: &RingBuffer, into: &RingBuffer) -> Mirror<NoWake> {
        Mirror {
            port: "imu".into(),
            from: from.view(NoWake).expect("a free reader slot"),
            into: into.writer(NoWake).expect("a free writer"),
            dropped: 0,
        }
    }

    pub(crate) fn region_bytes(ring: &RingBuffer) -> Vec<u8> {
        let (base, len) = ring.region();
        // SAFETY: the region is live, and no handle writes it during the copy.
        unsafe { core::slice::from_raw_parts(base, len) }.to_vec()
    }

    /// Where reader slot 0's cursor sits in a region of `config`: the first
    /// byte that claiming the slot changes.
    pub(crate) fn slot_cursor(config: Config) -> usize {
        let ring = RingBuffer::create_in_memory(config);
        let free = region_bytes(&ring);
        let _view = ring.view(NoWake).expect("a free reader slot");
        let claimed = region_bytes(&ring);
        let changed = free.iter().zip(&claimed).position(|(a, b)| a != b);
        changed.expect("claiming a slot writes its cursor")
    }

    fn records(view: &mut View<NoWake>) -> Vec<Vec<u8>> {
        view.drain()
            .map(|record| record.expect("whole").to_vec())
            .collect()
    }

    #[test]
    fn test_drain_stops_at_corrupt_record() {
        let (source, target) = (ring(256), ring(256));
        let mut received = target.view(NoWake).expect("a free reader slot");
        let mut copy = mirror(&source, &target);
        let writer = source.writer(NoWake).expect("a free writer");
        writer.try_write(b"first").expect("room");
        writer.try_write(b"second").expect("room");
        let (base, len) = source.region();
        // `first` fills one 16-byte header and one 16-byte payload.
        let second = len - source.config().capacity + 32;
        // SAFETY: the header is inside the region, and nothing reads it concurrently.
        unsafe { base.add(second).cast::<u64>().write(u64::from(u32::MAX)) };

        assert_eq!(copy.drain(), Err(Corrupt));
        assert_eq!(records(&mut received), [b"first".to_vec()]);
        assert_eq!(copy.dropped, 0);
    }

    #[test]
    fn test_drain_stops_at_corrupt_destination() {
        let (source, target) = (ring(256), ring(256));
        let mut copy = mirror(&source, &target);
        let writer = source.writer(NoWake).expect("a free writer");
        writer.try_write(b"first").expect("room");
        writer.try_write(b"second").expect("room");
        let (base, _) = target.region();
        let cursor = slot_cursor(target.config());
        // SAFETY: the slot is inside the region, and nothing reads it concurrently.
        unsafe { base.add(cursor).cast::<u64>().write(u64::MAX - 1) };

        assert_eq!(copy.drain(), Err(Corrupt));
        assert_eq!(copy.dropped, 0);
    }

    #[test]
    fn test_drain_counts_refused_records() {
        let (source, target) = (ring(256), ring(128));
        let _behind = target.view(NoWake).expect("a free reader slot");
        let mut copy = mirror(&source, &target);
        let writer = source.writer(NoWake).expect("a free writer");
        for _ in 0..8 {
            writer.try_write(&[7; 16]).expect("room");
        }
        assert_eq!(copy.drain(), Ok(()));
        // The target holds four 32-byte records, and its reader takes none.
        assert_eq!(copy.dropped, 4);
    }

    #[test]
    fn test_release_inputs_frees_reader_slot() {
        let (real, copy) = (ring(256), ring(256));
        let def = SystemDef {
            name: "nav".into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        };
        let mut mirrors = Mirrors::new(&def, vec![mirror(&real, &copy)], Vec::new());
        assert!(real.view(NoWake).is_err());
        mirrors.release_inputs();
        assert!(real.view(NoWake).is_ok());
        assert_eq!(mirrors.fields.len(), 1);
        assert_eq!(mirrors.drain_inputs(), Ok(()));
    }
}
