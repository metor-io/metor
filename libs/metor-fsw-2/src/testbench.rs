//! A single-entry test harness: drive one pack entry without a coordinator.
//!
//! Unit tests want to construct a system from params, feed its inputs typed
//! frames, step cycles, and read its outputs back — none of which needs
//! wiring, ring sizing passes, or a run loop. [`TestBench::new`] allocates
//! one heap ring per declared port straight off the entry's descriptor,
//! binds the entry over them through the same positional walk the real
//! builder uses, and hands the test typed write/read handles:
//!
//! ```ignore
//! let mut pack = Pack::new().system("nav", system(nav_execute).init(nav_init));
//! let mut bench = TestBench::new(pack.entry_mut("nav").unwrap(), &params)?;
//! bench.write::<Sensors>(&sensors);
//! bench.step(Timestamp(1));
//! let est = bench.read::<AttitudeEstimate>().expect("published");
//! ```
//!
//! The bench mounts the entry wired (no occupant tail) and holds one writer
//! per input and one view per output, addressed by frame type. Message
//! ports get the same treatment through [`send`](TestBench::send) /
//! [`recv`](TestBench::recv).

use metor_fsw_ring::{Config, NoWake, RingBuffer, View};
use metor_proto::types::Timestamp;
use serde::Serialize;
use serde::de::DeserializeOwned;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::binder::{AnySource, Binder, BoundInput, BoundPort};
use crate::descriptor::{PortDesc, PortId, SystemDescriptor};
use crate::frame::Frame;
use crate::message::{MsgOut, NamedMsg};
use crate::pack::{Driver, EntryParams, MakeError, Mount, PackEntry, StepStatus};
use crate::port::{Output, capacity_for};
use crate::registry::Registry;

/// One pack entry bound over bench-owned rings, steppable in isolation.
pub struct TestBench {
    driver: Box<dyn Driver>,
    descriptor: SystemDescriptor,
    /// One ring per descriptor input, in order; the bench holds the writer
    /// side of each.
    input_rings: Vec<RingBuffer>,
    /// One persistent view per descriptor output, claimed before the entry
    /// binds (a view starts at the live edge, so it must exist before data
    /// flows). Reads drain them, so each read sees records since the last.
    output_views: Vec<View<NoWake>>,
    /// The last step's status, so a test can assert a task's terminal.
    last_status: Option<StepStatus>,
}

fn ring_for(desc: &PortDesc) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: capacity_for(desc.max_size, crate::DEFAULT_DEPTH),
        // The entry itself plus a couple of bench readers.
        max_readers: 4,
    })
}

impl TestBench {
    /// Create the entry (decoding `params` as canonical postcard bytes; use
    /// `&[]` for a paramless entry), allocate a ring per declared port, and
    /// bind it exactly as the coordinator would.
    pub fn new(entry: &mut PackEntry, params: &[u8]) -> Result<Self, MakeError> {
        let pending = entry.create(EntryParams::Postcard(params))?;
        let descriptor = entry.descriptor().clone();
        let input_rings: Vec<RingBuffer> = descriptor.inputs.iter().map(ring_for).collect();
        let output_rings: Vec<RingBuffer> = descriptor.outputs.iter().map(ring_for).collect();
        // Claim the bench's read views before the entry binds or inits, so
        // nothing the entry publishes is ever ahead of them.
        let output_views: Vec<View<NoWake>> = output_rings
            .iter()
            .map(|r| r.view(NoWake).expect("bench reader slot"))
            .collect();

        // The positional walk over bench-owned ports, via the host Binder so
        // fan-in message inputs bind exactly as they would in a real graph.
        let outs: Vec<BoundPort> = output_rings
            .iter()
            .map(|r| BoundPort::new(r.clone()))
            .collect();
        let ins: Vec<BoundInput> = input_rings
            .iter()
            .map(|r| BoundInput::One(BoundPort::new(r.clone())))
            .collect();
        let registry = std::sync::Arc::new(Registry::new(Vec::new()));
        let mut binder = Binder::new(&outs, &ins, registry);
        let mut src = AnySource::Host(&mut binder);
        let mut driver = pending(&mut src, Mount::Wired);
        driver.init();
        Ok(Self {
            driver,
            descriptor,
            input_rings,
            output_views,
            last_status: None,
        })
    }

    /// The entry's descriptor, for asserting port shapes.
    pub fn descriptor(&self) -> &SystemDescriptor {
        &self.descriptor
    }

    /// Run one cycle at `now`.
    pub fn step(&mut self, now: Timestamp) {
        self.last_status = Some(self.driver.step(now));
    }

    /// Whether the last [`step`](Self::step) reported the entry done (a task
    /// whose future returned), and with what outcome.
    pub fn done(&self) -> Option<crate::sequence::Outcome> {
        match self.last_status {
            Some(StepStatus::Done(outcome)) => Some(outcome),
            _ => None,
        }
    }

    fn input_ring(&self, id: PortId, what: &str) -> &RingBuffer {
        let idx = self
            .descriptor
            .inputs
            .iter()
            .position(|p| p.id() == id)
            .unwrap_or_else(|| panic!("the entry declares no {what} input"));
        &self.input_rings[idx]
    }

    fn output_view(&mut self, id: PortId, what: &str) -> &mut View<NoWake> {
        let idx = self
            .descriptor
            .outputs
            .iter()
            .position(|p| p.id() == id)
            .unwrap_or_else(|| panic!("the entry declares no {what} output"));
        &mut self.output_views[idx]
    }

    /// Write one frame into the entry's `F` input, as its upstream producer
    /// would.
    pub fn write<F>(&mut self, frame: &F)
    where
        F: Frame + IntoBytes + Immutable,
    {
        let ring = self.input_ring(PortId::Component(F::FRAME_ID), F::NAME);
        // The writer claim frees on drop, so a per-call writer keeps the
        // bench simple; the entry only ever holds views on its inputs.
        let mut out: Output<F> =
            Output::new(ring.writer(NoWake).expect("bench input writer"));
        out.write(frame).expect("bench input ring has room");
    }

    /// Read the entry's newest `F` output frame published since the last
    /// read, cloned out so the bench can keep stepping.
    pub fn read<F>(&mut self) -> Option<F>
    where
        F: Frame + FromBytes + KnownLayout + Immutable + Clone,
    {
        let view = self.output_view(PortId::Component(F::FRAME_ID), F::NAME);
        let mut newest: Option<F> = None;
        crate::port::drain_view(view, |bytes| {
            if let Ok((frame, _)) = F::ref_from_prefix(bytes) {
                newest = Some(frame.clone());
            }
        })
        .expect("bench view never laps (the ring is lossless)");
        newest
    }

    /// Send one message into the entry's `M` input.
    pub fn send<M>(&mut self, msg: &M)
    where
        M: NamedMsg + Serialize,
    {
        let ring = self.input_ring(PortId::Packet(M::ID), M::NAME);
        let mut out: MsgOut<M> =
            MsgOut::new(ring.writer(NoWake).expect("bench msg writer"));
        out.emit(msg).expect("bench msg ring has room");
    }

    /// Drain every message the entry has emitted on its `M` output since the
    /// last call.
    pub fn recv<M>(&mut self) -> Vec<M>
    where
        M: NamedMsg + DeserializeOwned,
    {
        let view = self.output_view(PortId::Packet(M::ID), M::NAME);
        let mut msgs = Vec::new();
        crate::port::drain_view(view, |bytes| {
            if let Some((id, payload)) = crate::message::split_record(bytes)
                && id == M::ID
                && let Ok(msg) = postcard::from_bytes::<M>(payload)
            {
                msgs.push(msg);
            }
        })
        .expect("bench view never laps (the ring is lossless)");
        msgs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Input, Pack, system};

    #[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default, Clone)]
    #[repr(C)]
    #[metor_fsw(name = "tb_in")]
    struct TbIn {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        value: f64,
    }

    #[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default, Clone)]
    #[repr(C)]
    #[metor_fsw(name = "tb_out")]
    struct TbOut {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        doubled: f64,
    }

    fn double(_s: &mut (), now: Timestamp, i: &mut Input<TbIn>, o: &mut Output<TbOut>) {
        if let Some(r) = i.latest() {
            o.publish(&TbOut {
                timestamp: now,
                doubled: r.value * 2.0,
            });
        }
    }

    /// The whole point: params → typed input → step → typed output, no
    /// coordinator anywhere.
    #[test]
    fn bench_drives_one_entry() {
        let mut pack = Pack::new().system("double", system(double).init(|| ()));
        let mut bench =
            TestBench::new(pack.entry_mut("double").unwrap(), &[]).expect("bench builds");

        bench.write(&TbIn {
            timestamp: Timestamp(0),
            value: 21.0,
        });
        bench.step(Timestamp(5));

        let out = bench.read::<TbOut>().expect("published");
        assert_eq!(out.doubled, 42.0);
        assert_eq!(out.timestamp, Timestamp(5), "stamped with the bench now");
        assert!(bench.done().is_none(), "a cyclic entry never completes");
    }

    /// A task entry runs to its outcome on the bench clock.
    #[test]
    fn bench_drives_a_task_to_done() {
        use crate::sequence::{Outcome, wait};
        use std::time::Duration;

        async fn nap() -> Outcome {
            wait(Duration::from_micros(2)).await;
            Outcome::Completed
        }

        let mut pack = Pack::new().task("nap", nap);
        let mut bench = TestBench::new(pack.entry_mut("nap").unwrap(), &[]).expect("bench builds");

        bench.step(Timestamp(0));
        assert!(bench.done().is_none(), "still waiting");
        bench.step(Timestamp(2));
        assert_eq!(bench.done(), Some(Outcome::Completed));
    }
}
