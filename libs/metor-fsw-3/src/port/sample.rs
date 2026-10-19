//! [`Sample`], an input that is read once per cycle and then held.

use core::cell::Cell;

use super::{Input, InputWake};
use crate::frame::Frame;
use crate::log::Log;

/// Holds an input's newest record.
///
/// The value is refreshed once per cycle, before the system runs. It keeps the
/// last record until a newer one arrives.
pub struct Sample<T: Frame + Copy> {
    value: Cell<Option<T>>,
}

impl<T: Frame + Copy> Sample<T> {
    /// Returns the newest record as of this cycle.
    ///
    /// Returns `None` until the first record arrives.
    pub fn get(&self) -> Option<T> {
        self.value.get()
    }

    /// Stores the newest record from `input`.
    ///
    /// Keeps the previous value if nothing arrived or the read failed.
    pub(crate) fn refresh<W: InputWake>(&self, input: &mut Input<T, W>) {
        match input.latest() {
            Ok(Some(record)) => self.value.set(Some(*record)),
            Ok(None) => {}
            Err(err) => Log.fault("sample", err.to_string()),
        }
    }
}

/// A bound [`Sample`] and the input that fills it.
pub struct SampledInput<T: Frame + Copy, W: InputWake> {
    pub(crate) input: Input<T, W>,
    pub(crate) sample: Sample<T>,
}

impl<T: Frame + Copy, W: InputWake> SampledInput<T, W> {
    /// Wraps `input`, with no value until the first refresh.
    pub(crate) fn new(input: Input<T, W>) -> Self {
        Self {
            input,
            sample: Sample {
                value: Cell::new(None),
            },
        }
    }

    /// Stores the input's newest record in the sample.
    pub(crate) fn refresh(&mut self) {
        self.sample.refresh(&mut self.input);
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{NoWake, RingBuffer, Writer};
    use metor_proto::types::Timestamp;
    use metor_proto_wkt::LogEvent;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::log::{LogPort, with_log_port};
    use crate::tests::utils::{Imu, input, pair, ring};

    /// A writer onto `ring` and the sample its one view fills.
    fn sampled(ring: &RingBuffer) -> (Writer<NoWake>, SampledInput<Imu, NoWake>) {
        let writer = ring.writer(NoWake).expect("free writer");
        (writer, SampledInput::new(input::<Imu>(ring)))
    }

    fn write(writer: &mut Writer<NoWake>, imu: Imu) {
        writer.try_write(imu.as_bytes()).expect("ring has room");
    }

    #[test]
    fn test_none_before_first_then_newest() {
        let ring = ring::<Imu>(8, 1);
        let (mut writer, mut sampled) = sampled(&ring);
        sampled.refresh();
        assert_eq!(sampled.sample.get(), None);
        write(&mut writer, Imu::new(1, 1.0));
        assert_eq!(sampled.sample.get(), None, "held until the next refresh");
        sampled.refresh();
        assert_eq!(sampled.sample.get(), Some(Imu::new(1, 1.0)));
    }

    #[test]
    fn test_quiet_cycle_keeps_value() {
        let ring = ring::<Imu>(8, 1);
        let (mut writer, mut sampled) = sampled(&ring);
        write(&mut writer, Imu::new(1, 1.0));
        sampled.refresh();
        sampled.refresh();
        assert_eq!(sampled.sample.get(), Some(Imu::new(1, 1.0)));
    }

    #[test]
    fn test_takes_last_of_a_cycle() {
        let ring = ring::<Imu>(8, 1);
        let (mut writer, mut sampled) = sampled(&ring);
        for t in 1..=3 {
            write(&mut writer, Imu::new(t, t as f64));
        }
        sampled.refresh();
        assert_eq!(sampled.sample.get(), Some(Imu::new(3, 3.0)));
    }

    #[test]
    fn test_unread_sample_never_blocks_writer() {
        let ring = ring::<Imu>(2, 1);
        let (mut writer, mut sampled) = sampled(&ring);
        for t in 0..1000 {
            write(&mut writer, Imu::new(t, t as f64));
            sampled.refresh();
        }
        assert_eq!(sampled.sample.get(), Some(Imu::new(999, 999.0)));
    }

    #[test]
    fn test_decode_error_keeps_value() {
        let (_log_ring, output, mut lines) = pair::<LogEvent>(4);
        let mut port = LogPort::new(output);

        let ring = ring::<Imu>(8, 1);
        let (mut writer, mut sampled) = sampled(&ring);
        write(&mut writer, Imu::new(1, 1.0));
        sampled.refresh();
        writer.try_write(&[0u8; 4]).expect("ring has room");
        with_log_port(&mut port, Timestamp(2), || sampled.refresh());
        assert_eq!(sampled.sample.get(), Some(Imu::new(1, 1.0)));
        let line = lines.drain().next().expect("a line").expect("decodes");
        assert_eq!(line.fields, vec![("kind".into(), "sample".into())]);

        write(&mut writer, Imu::new(3, 3.0));
        sampled.refresh();
        assert_eq!(sampled.sample.get(), Some(Imu::new(3, 3.0)));
    }
}
