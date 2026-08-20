//! Delivery-aware record forwarding across an isolated system boundary.
//!
//! The owner decides *when* imports and exports run. A wasm occupant brackets
//! its synchronous execute call with them; an async boundary runs both at the
//! system's position in the cycle while the task itself runs between cycles.

use metor_fsw_2_core::Delivery;
use metor_fsw_ring::{NoWake, ReadError, View, WakeSource, Writer};

/// One source ring forwarded into one destination ring according to the
/// source port's delivery contract.
pub(crate) struct RingPump<W: WakeSource> {
    from: View<NoWake>,
    to: Writer<W>,
    delivery: Delivery,
    /// Source `committed` observed at the last snapshot forward.
    last_committed: u64,
}

impl<W: WakeSource> RingPump<W> {
    pub(crate) fn new(from: View<NoWake>, to: Writer<W>, delivery: Delivery) -> Self {
        Self {
            from,
            to,
            delivery,
            last_committed: u64::MAX,
        }
    }

    /// Forward what this delivery mode owes and return destination drops.
    pub(crate) fn pump(&mut self) -> Result<u64, ReadError> {
        match self.delivery {
            Delivery::Log => {
                let mut dropped = 0;
                loop {
                    match self.from.try_read()? {
                        Some(grant) => {
                            dropped += u64::from(self.to.try_write(&grant).is_err());
                        }
                        None => return Ok(dropped),
                    }
                }
            }
            Delivery::Snapshot => {
                let committed = self.from.committed();
                if committed == self.last_committed {
                    return Ok(0);
                }
                let dropped = match self.from.try_latest()? {
                    Some(grant) => u64::from(self.to.try_write(&grant).is_err()),
                    None => 0,
                };
                self.last_committed = committed;
                Ok(dropped)
            }
        }
    }
}

/// Bidirectional I/O for one isolated cycle participant.
pub(crate) struct IoBridge<I: WakeSource, O: WakeSource> {
    imports: Vec<RingPump<I>>,
    exports: Vec<RingPump<O>>,
    dropped: u64,
}

impl<I: WakeSource, O: WakeSource> IoBridge<I, O> {
    pub(crate) fn new(imports: Vec<RingPump<I>>, exports: Vec<RingPump<O>>) -> Self {
        Self {
            imports,
            exports,
            dropped: 0,
        }
    }

    pub(crate) fn import(&mut self) -> Result<(), ReadError> {
        for pump in &mut self.imports {
            self.dropped += pump.pump()?;
        }
        Ok(())
    }

    pub(crate) fn export(&mut self) -> Result<(), ReadError> {
        for pump in &mut self.exports {
            self.dropped += pump.pump()?;
        }
        Ok(())
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    pub(crate) fn drain_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_fsw_ring::{Config, RingBuffer};

    fn ring(capacity: usize) -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity,
            max_readers: 2,
        })
    }

    #[test]
    fn corrupt_source_is_an_error() {
        let source = ring(64);
        let view = source.view(NoWake).expect("view before commit");
        let mut writer = source.writer(NoWake).expect("writer");
        writer.try_write(&[1; 8]).expect("commit");

        // RegionHeader::data_offset is the u64 at byte offset 16. Corrupt the
        // first record's length word after publication.
        let (base, _) = source.region();
        let data_offset = unsafe { (base.add(16) as *const u64).read() } as usize;
        unsafe { (base.add(data_offset) as *mut u64).write(u64::MAX) };

        let destination = ring(64);
        let to = destination.writer(NoWake).expect("destination writer");
        let mut pump = RingPump::new(view, to, Delivery::Log);
        assert_eq!(pump.pump(), Err(ReadError::Corrupt));
    }

    #[test]
    fn destination_backpressure_is_counted_and_drained_once() {
        let source = ring(64);
        let view = source.view(NoWake).expect("source view");
        let mut source_writer = source.writer(NoWake).expect("source writer");
        let destination = ring(16);
        let _blocked_reader = destination.view(NoWake).expect("blocked reader");
        let to = destination.writer(NoWake).expect("destination writer");
        let pump = RingPump::new(view, to, Delivery::Log);
        let mut io = IoBridge::<NoWake, NoWake>::new(vec![pump], Vec::new());

        source_writer
            .try_write(&[1; 8])
            .expect("first source record");
        io.import().expect("first import");
        source_writer
            .try_write(&[2; 8])
            .expect("second source record");
        io.import().expect("second import");
        assert_eq!(io.drain_dropped(), 1);
        assert_eq!(io.drain_dropped(), 0);
    }
}
