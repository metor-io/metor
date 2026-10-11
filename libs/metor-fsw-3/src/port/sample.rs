//! [`Sample`], an input that is read once per cycle and then held.

use core::cell::Cell;
use std::rc::Rc;

use metor_fsw_3_ring::WakeSink;

use super::Input;
use crate::frame::Frame;
use crate::log::Log;

/// Holds an input's newest record.
///
/// The value is refreshed once per cycle, before the system runs. It keeps the
/// last record until a newer one arrives.
pub struct Sample<T: Frame + Copy> {
    value: Rc<Cell<Option<T>>>,
}

impl<T: Frame + Copy + 'static> Sample<T> {
    /// Creates a `Sample` and the sampler that fills it from `input`.
    pub(crate) fn bind<W: WakeSink + 'static>(input: Input<T, W>) -> (Self, Box<dyn Sampler>) {
        let value = Rc::new(Cell::new(None));
        let taker = Taker {
            input,
            value: value.clone(),
        };
        (Self { value }, Box::new(taker))
    }

    /// Returns the newest record as of this cycle.
    ///
    /// Returns `None` until the first record arrives.
    pub fn get(&self) -> Option<T> {
        self.value.get()
    }
}

/// Refreshes one [`Sample`] for the cycle.
pub(crate) trait Sampler {
    fn take(&mut self);
}

struct Taker<T: Frame + Copy, W: WakeSink> {
    input: Input<T, W>,
    value: Rc<Cell<Option<T>>>,
}

impl<T: Frame + Copy, W: WakeSink> Sampler for Taker<T, W> {
    /// Stores the newest record.
    ///
    /// Keeps the previous value if nothing arrived or the read failed.
    fn take(&mut self) {
        match self.input.latest() {
            Ok(Some(record)) => self.value.set(Some(*record)),
            Ok(None) => {}
            Err(err) => Log.fault("sample", err.to_string()),
        }
    }
}

/// The samplers one step refreshes each cycle.
#[derive(Default)]
pub(crate) struct Samplers(Vec<Box<dyn Sampler>>);

impl Samplers {
    pub(crate) fn push(&mut self, sampler: Box<dyn Sampler>) {
        self.0.push(sampler);
    }

    pub(crate) fn take_all(&mut self) {
        for sampler in &mut self.0 {
            sampler.take();
        }
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
    fn sampled(ring: &RingBuffer) -> (Writer<NoWake>, Sample<Imu>, Box<dyn Sampler>) {
        let writer = ring.writer(NoWake).expect("free writer");
        let (sample, sampler) = Sample::bind(input::<Imu>(ring));
        (writer, sample, sampler)
    }

    fn write(writer: &mut Writer<NoWake>, imu: Imu) {
        writer.try_write(imu.as_bytes()).expect("ring has room");
    }

    #[test]
    fn test_none_before_first_then_newest() {
        let ring = ring::<Imu>(8, 1);
        let (mut writer, sample, mut sampler) = sampled(&ring);
        sampler.take();
        assert_eq!(sample.get(), None);
        write(&mut writer, Imu::new(1, 1.0));
        assert_eq!(sample.get(), None, "held until the next take");
        sampler.take();
        assert_eq!(sample.get(), Some(Imu::new(1, 1.0)));
    }

    #[test]
    fn test_quiet_cycle_keeps_value() {
        let ring = ring::<Imu>(8, 1);
        let (mut writer, sample, mut sampler) = sampled(&ring);
        write(&mut writer, Imu::new(1, 1.0));
        sampler.take();
        sampler.take();
        assert_eq!(sample.get(), Some(Imu::new(1, 1.0)));
    }

    #[test]
    fn test_takes_last_of_a_cycle() {
        let ring = ring::<Imu>(8, 1);
        let (mut writer, sample, mut sampler) = sampled(&ring);
        for t in 1..=3 {
            write(&mut writer, Imu::new(t, t as f64));
        }
        sampler.take();
        assert_eq!(sample.get(), Some(Imu::new(3, 3.0)));
    }

    #[test]
    fn test_unread_sample_never_blocks_writer() {
        let ring = ring::<Imu>(2, 1);
        let (mut writer, sample, mut sampler) = sampled(&ring);
        for t in 0..1000 {
            write(&mut writer, Imu::new(t, t as f64));
            sampler.take();
        }
        assert_eq!(sample.get(), Some(Imu::new(999, 999.0)));
    }

    #[test]
    fn test_decode_error_keeps_value() {
        let (_log_ring, output, mut lines) = pair::<LogEvent>(4);
        let mut port = LogPort::new(output);

        let ring = ring::<Imu>(8, 1);
        let (mut writer, sample, mut sampler) = sampled(&ring);
        write(&mut writer, Imu::new(1, 1.0));
        sampler.take();
        writer.try_write(&[0u8; 4]).expect("ring has room");
        with_log_port(&mut port, Timestamp(2), || sampler.take());
        assert_eq!(sample.get(), Some(Imu::new(1, 1.0)));
        let line = lines.drain().next().expect("a line").expect("decodes");
        assert_eq!(line.fields, vec![("kind".into(), "sample".into())]);

        write(&mut writer, Imu::new(3, 3.0));
        sampler.take();
        assert_eq!(sample.get(), Some(Imu::new(3, 3.0)));
    }
}
