//! Mirrors copy records between rings across an adapter boundary.
//!
//! A system that runs on its own thread or in a wasm guest does not touch the
//! real rings directly. Instead, it gets its own copy of each port's ring, and
//! each cycle the adapter mirrors the real inputs into those copies and the
//! copies back out to the real outputs.
//! A copy that is full drops records, and the mirrors periodically log how
//! many each port dropped.

use core::fmt::Write;
use std::borrow::Cow;

use metor_fsw_3_ring::{Config, NoWake, ReadError, View, WakeSource, WriteError, Writer};
use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};

use crate::coordinator::ParamError;
use crate::fn_system::LOG_PORT;
use crate::record::Record;
use crate::system::SystemDef;

/// How many cycles pass between drop reports. Drop counts add up in between.
const DROP_REPORT_CYCLES: u64 = 100;

/// The key/value fields of a log line, kept between cycles so they can be
/// reused.
type Fields = Vec<(Cow<'static, str>, Cow<'static, str>)>;

/// A ring on either side of a mirror is corrupt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Corrupt;

impl From<ReadError> for Corrupt {
    fn from(ReadError::Corrupt: ReadError) -> Self {
        Corrupt
    }
}

/// Copies records from one ring into another.
///
/// Records that do not fit in the destination are dropped and counted.
pub(crate) struct Mirror<W: WakeSource> {
    port: Cow<'static, str>,
    from: View<NoWake>,
    into: Writer<W>,
    dropped: u64,
}

impl<W: WakeSource> Mirror<W> {
    /// Creates a mirror from `from` into `into`. Drops are reported under the
    /// name `port`.
    pub(crate) fn new(port: Cow<'static, str>, from: View<NoWake>, into: Writer<W>) -> Self {
        Self {
            port,
            from,
            into,
            dropped: 0,
        }
    }

    /// Copies every waiting record, counting the ones that did not fit.
    ///
    /// Stops at the first sign of corruption in either ring. Records copied
    /// before that point stay copied.
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

/// The mirrors for every port of one system.
///
/// Input mirrors copy the real input rings into the system's copies. Output
/// mirrors copy the system's copies into the real output rings. Drop reports
/// and adapter log lines go out on the system's `log` output.
pub(crate) struct Mirrors<W: WakeSource> {
    inputs: Vec<Mirror<W>>,
    outputs: Vec<Mirror<NoWake>>,
    /// The index of the system's `log` output. The adapter writes its own
    /// log lines there too.
    log: Option<usize>,
    line: Vec<u8>,
    /// One field per port. Counts are written into these in place, so a drop
    /// report does not allocate.
    fields: Fields,
    since_report: u64,
}

impl<W: WakeSource> Mirrors<W> {
    /// Groups the mirrors for `def`'s ports. Both lists are in bind order.
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

    /// Drains every input mirror. Returns an error if any ring was corrupt.
    pub(crate) fn drain_inputs(&mut self) -> Result<(), Corrupt> {
        drain(&mut self.inputs)
    }

    /// Drains every output mirror. Returns an error if any ring was corrupt.
    pub(crate) fn drain_outputs(&mut self) -> Result<(), Corrupt> {
        drain(&mut self.outputs)
    }

    /// Counts one cycle, and logs the drops every [`DROP_REPORT_CYCLES`]
    /// cycles.
    ///
    /// The log line lists how many records each port dropped since the last
    /// report. Nothing is logged if no port dropped anything.
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

    /// Writes a log line with a `kind` field to the system's `log` output.
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

    /// Moves each port's drop count into its field and resets the count.
    /// Returns true if any port dropped a record.
    fn take_drops(&mut self) -> bool {
        let mut dropped = false;
        let counts = counts(&mut self.inputs, &mut self.outputs);
        for ((_, field), count) in self.fields[1..].iter_mut().zip(counts) {
            dropped |= count > 0;
            set_count(field, count);
        }
        dropped
    }

    /// Writes `event` to the system's `log` output, if the system has one.
    fn emit(&mut self, event: &LogEvent) {
        let Some(log) = self.log else { return };
        let Ok(bytes) = event.encode(&mut self.line) else {
            return;
        };
        let _ = self.outputs[log].into.try_write(bytes);
    }
}

#[cfg(feature = "host")]
impl<W: WakeSource> Mirrors<W> {
    /// Drops the input mirrors, which frees their reader slots on the real
    /// rings.
    pub(crate) fn release_inputs(&mut self) {
        self.fields.drain(1..=self.inputs.len());
        self.inputs.clear();
    }
}

/// Drains every mirror in `mirrors`. Returns an error if any ring was
/// corrupt.
fn drain<W: WakeSource>(mirrors: &mut [Mirror<W>]) -> Result<(), Corrupt> {
    mirrors
        .iter_mut()
        .map(Mirror::drain)
        .fold(Ok(()), Result::and)
}

/// Takes each mirror's drop count, inputs first, resetting each to zero.
fn counts<'a, W: WakeSource>(
    inputs: &'a mut [Mirror<W>],
    outputs: &'a mut [Mirror<NoWake>],
) -> impl Iterator<Item = u64> + 'a {
    let inputs = inputs.iter_mut().map(|m| core::mem::take(&mut m.dropped));
    let outputs = outputs.iter_mut().map(|m| core::mem::take(&mut m.dropped));
    inputs.chain(outputs)
}

/// Writes `count` into a field's existing buffer, without allocating.
fn set_count(value: &mut Cow<'static, str>, count: u64) {
    let Cow::Owned(text) = value else { return };
    text.clear();
    let _ = write!(text, "{count}");
}

/// Builds the fields of a drop report: a `kind` field, then one count field
/// per port in bind order.
fn drop_fields<'a>(ports: impl Iterator<Item = &'a Cow<'static, str>>) -> Fields {
    let kind = (Cow::Borrowed("kind"), Cow::Borrowed("mirror_dropped"));
    let counts = ports.map(|port| {
        let count = Cow::Owned(String::with_capacity(20));
        (port.clone(), count)
    });
    core::iter::once(kind).chain(counts).collect()
}

/// Returns the config for a mirror ring and the length of its region.
///
/// The mirror has `readers` readers and `factor` times the capacity of the
/// `capacity`-byte ring it copies.
pub(crate) fn mirror_config(
    capacity: usize,
    factor: usize,
    readers: usize,
) -> Result<(Config, usize), ParamError> {
    let too_large = ParamError::MirrorTooLarge { capacity };
    let config = Config {
        capacity: capacity.checked_mul(factor).ok_or(too_large.clone())?,
        max_readers: readers,
    };
    let len = metor_fsw_3_ring::checked_region_len(&config).ok_or(too_large)?;
    Ok((config, len))
}

/// Returns the index of the system's `log` output, if it has one.
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
        Mirror::new(
            "imu".into(),
            from.view(NoWake).expect("a free reader slot"),
            into.writer(NoWake).expect("a free writer"),
        )
    }

    pub(crate) fn region_bytes(ring: &RingBuffer) -> Vec<u8> {
        let (base, len) = ring.region();
        // SAFETY: the region is live, and no handle writes it during the copy.
        unsafe { core::slice::from_raw_parts(base, len) }.to_vec()
    }

    /// Returns the offset of reader slot 0's cursor in a region of `config`.
    ///
    /// It finds the cursor as the first byte that changes when the slot is
    /// claimed.
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
        // The target fits four 32-byte records, and its reader never reads.
        assert_eq!(copy.dropped, 4);
    }

    #[cfg(feature = "host")]
    #[test]
    fn test_release_inputs_frees_reader_slot() {
        let (real, copy) = (ring(256), ring(256));
        let def = SystemDef {
            name: "nav".into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            grants: Vec::new(),
        };
        let mut mirrors = Mirrors::new(&def, vec![mirror(&real, &copy)], Vec::new());
        assert!(real.view(NoWake).is_err());
        mirrors.release_inputs();
        assert!(real.view(NoWake).is_ok());
        assert_eq!(mirrors.fields.len(), 1);
        assert_eq!(mirrors.drain_inputs(), Ok(()));
    }

    #[test]
    fn test_mirror_takes_its_readers() {
        let config = |readers| mirror_config(64, 4, readers).unwrap().0;
        let two = RingBuffer::create_in_memory(config(2));
        let _first = two.view(NoWake).unwrap();
        assert!(two.view(NoWake).is_ok(), "a second reader");
        let one = RingBuffer::create_in_memory(config(1));
        let _first = one.view(NoWake).unwrap();
        assert!(one.view(NoWake).is_err(), "only one");
    }

    #[test]
    fn test_reject_oversize_mirror() {
        assert!(mirror_config(64, 4, 1).is_ok());
        // A capacity that is not a valid ring size once multiplied by four,
        // and one whose multiple overflows.
        for capacity in [3, usize::MAX / 2] {
            assert_eq!(
                mirror_config(capacity, 4, 1).err(),
                Some(ParamError::MirrorTooLarge { capacity })
            );
        }
    }

    #[test]
    fn test_mirror_drain_snapshot() {
        use core::cell::RefCell;

        struct Refill(RefCell<Writer<NoWake>>);
        impl WakeSource for Refill {
            fn notify(&self) {
                self.0
                    .borrow_mut()
                    .try_write(b"later")
                    .expect("source has room");
            }
        }

        let source = RingBuffer::create_in_memory(Config {
            capacity: 256,
            max_readers: 1,
        });
        let target = RingBuffer::create_in_memory(Config {
            capacity: 256,
            max_readers: 1,
        });
        let writer = source.writer(NoWake).expect("writer");
        let mut received = target.view(NoWake).expect("reader");
        let from = source.view(NoWake).expect("reader");
        writer.try_write(b"first").expect("room");
        writer.try_write(b"second").expect("room");
        let into = target.writer(Refill(RefCell::new(writer))).expect("writer");
        let mut mirror = Mirror::new("out".into(), from, into);
        mirror.drain().expect("whole records");
        assert_eq!(
            received
                .drain()
                .collect::<Result<Vec<_>, _>>()
                .expect("valid"),
            [b"first".as_slice(), b"second".as_slice()]
        );
        assert_eq!(
            mirror
                .from
                .drain()
                .collect::<Result<Vec<_>, _>>()
                .expect("valid"),
            [b"later".as_slice(), b"later".as_slice()]
        );
        assert_eq!(mirror.dropped, 0);
    }
}
