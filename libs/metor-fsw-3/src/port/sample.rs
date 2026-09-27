//! [`Sample`], an input read once per cycle and held.

use core::cell::Cell;
use std::rc::Rc;

use metor_fsw_3_ring::WakeSink;

use super::Input;
use crate::frame::Frame;
use crate::log::Log;

/// The newest record of an input as of this cycle, held until a newer one arrives.
pub struct Sample<T: Frame + Copy> {
    value: Rc<Cell<Option<T>>>,
}

impl<T: Frame + Copy + 'static> Sample<T> {
    /// Moves `input` into the sampler that fills the returned `Sample`.
    pub(crate) fn bind<W: WakeSink + 'static>(input: Input<T, W>) -> (Self, Box<dyn Sampler>) {
        let value = Rc::new(Cell::new(None));
        let taker = Taker {
            input,
            value: value.clone(),
        };
        (Self { value }, Box::new(taker))
    }

    /// This cycle's value: the newest record so far, or `None` before the first.
    pub fn get(&self) -> Option<T> {
        self.value.get()
    }
}

/// A `Sampler` takes one input's sample for the cycle.
pub(crate) trait Sampler {
    fn take(&mut self);
}

struct Taker<T: Frame + Copy, W: WakeSink> {
    input: Input<T, W>,
    value: Rc<Cell<Option<T>>>,
}

impl<T: Frame + Copy, W: WakeSink> Sampler for Taker<T, W> {
    /// Holds the newest record, keeping the last one when none arrived or it
    /// failed to read.
    fn take(&mut self) {
        match self.input.latest() {
            Ok(Some(record)) => self.value.set(Some(*record)),
            Ok(None) => {}
            Err(err) => Log.fault("sample", err.to_string()),
        }
    }
}

/// The samplers one step takes each cycle.
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
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer, Writer};
    use metor_proto::types::Timestamp;
    use metor_proto_wkt::LogEvent;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::log::{LogPort, with_log_port};
    use crate::port::{Output, ring_capacity};
    use crate::record::Record;
    use crate::tests::utils::Imu;

    fn ring(depth: usize) -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(Imu::MAX_LEN, depth).expect("valid capacity"),
            max_readers: 1,
        })
    }

    /// A writer onto `ring` and the sample its one view fills.
    fn sampled(ring: &RingBuffer) -> (Writer<NoWake>, Sample<Imu>, Box<dyn Sampler>) {
        let writer = ring.writer(NoWake).expect("free writer");
        let input = Input::<Imu>::new(vec![ring.view(NoWake).expect("free slot")]);
        let (sample, sampler) = Sample::bind(input);
        (writer, sample, sampler)
    }

    fn write(writer: &mut Writer<NoWake>, imu: Imu) {
        writer.try_write(imu.as_bytes()).expect("ring has room");
    }

    #[test]
    fn test_none_before_first_then_newest() {
        let ring = ring(8);
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
        let ring = ring(8);
        let (mut writer, sample, mut sampler) = sampled(&ring);
        write(&mut writer, Imu::new(1, 1.0));
        sampler.take();
        sampler.take();
        assert_eq!(sample.get(), Some(Imu::new(1, 1.0)));
    }

    #[test]
    fn test_takes_last_of_a_cycle() {
        let ring = ring(8);
        let (mut writer, sample, mut sampler) = sampled(&ring);
        for t in 1..=3 {
            write(&mut writer, Imu::new(t, t as f64));
        }
        sampler.take();
        assert_eq!(sample.get(), Some(Imu::new(3, 3.0)));
    }

    #[test]
    fn test_unread_sample_never_blocks_writer() {
        let ring = ring(2);
        let (mut writer, sample, mut sampler) = sampled(&ring);
        for t in 0..1000 {
            write(&mut writer, Imu::new(t, t as f64));
            sampler.take();
        }
        assert_eq!(sample.get(), Some(Imu::new(999, 999.0)));
    }

    #[test]
    fn test_decode_error_keeps_value() {
        let log_ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(LogEvent::MAX_LEN, 4).expect("valid capacity"),
            max_readers: 1,
        });
        let mut port = LogPort::new(Output::new(log_ring.writer(NoWake).expect("free writer")));
        let mut lines = Input::<LogEvent>::new(vec![log_ring.view(NoWake).expect("free slot")]);

        let ring = ring(8);
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
