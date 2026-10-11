use std::cell::{Cell, RefCell};
use std::rc::Rc;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;

use super::run::Runner;
use super::{Clock, Coordinator, Entry, Step, SystemStatus};
use crate::Record;
use crate::port::{Input, Output, ring_capacity};
use crate::system::{System, SystemDef};
use crate::tests::utils::Imu;

#[derive(Default)]
struct Calls {
    execute: Cell<usize>,
    fault: Cell<usize>,
    drop: Cell<usize>,
}

#[derive(crate::SystemInputs)]
struct ConsumerInputs {
    data: Input<Imu>,
    status: Input<SystemStatus>,
}

struct FailingConsumer {
    calls: Rc<Calls>,
    fault_panics: bool,
    drop_panics: bool,
}

impl System for FailingConsumer {
    type State = ();
    type Inputs = ConsumerInputs;
    type Outputs = ();

    fn def() -> SystemDef {
        SystemDef::new::<ConsumerInputs, ()>("failed")
    }

    fn execute(&self, _: Timestamp, _: &mut (), inputs: &mut ConsumerInputs, _: &mut ()) {
        self.calls.execute.set(self.calls.execute.get() + 1);
        assert!(inputs.data.latest().expect("valid data").is_some());
        assert!(inputs.status.latest().expect("valid status").is_some());
        // PANIC Safety: the coordinator catches the failed cycle.
        panic!("consumer failed");
    }

    fn fault(&self, _: Timestamp, _: &mut (), message: &str) {
        self.calls.fault.set(self.calls.fault.get() + 1);
        assert_eq!(message, "consumer failed");
        if self.fault_panics {
            // PANIC Safety: fault reporting is contained separately.
            panic!("fault hook failed");
        }
    }
}

impl Drop for FailingConsumer {
    fn drop(&mut self) {
        self.calls.drop.set(self.calls.drop.get() + 1);
        if self.drop_panics {
            // PANIC Safety: retirement catches a destructor panic.
            panic!("destructor failed");
        }
    }
}

struct Producer(Output<Imu>);

impl Step for Producer {
    fn execute(&mut self, now: Timestamp) {
        self.0
            .write(&Imu::new(now.0, now.0 as f64))
            .expect("retired readers cannot block the producer");
    }
}

struct HealthyConsumer {
    inputs: ConsumerInputs,
    seen: Rc<RefCell<Vec<(Timestamp, Timestamp)>>>,
}

impl Step for HealthyConsumer {
    fn execute(&mut self, _: Timestamp) {
        let data = self
            .inputs
            .data
            .latest()
            .expect("valid data")
            .expect("data");
        let status = self
            .inputs
            .status
            .latest()
            .expect("valid status")
            .expect("status");
        self.seen
            .borrow_mut()
            .push((data.timestamp, status.timestamp));
    }
}

fn ring<T: Record>(readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: ring_capacity(T::MAX_LEN, 2).expect("two records fit"),
        max_readers: readers,
    })
}

fn input<T: Record>(ring: &RingBuffer) -> Input<T> {
    Input::try_new(vec![ring.view(NoWake).expect("available reader")]).expect("aligned")
}

fn output<T: Record>(ring: &RingBuffer) -> Output<T> {
    Output::try_new(ring.writer(NoWake).expect("available writer")).expect("aligned")
}

fn entry(name: &str, step: impl Step + 'static, status: &RingBuffer) -> Entry {
    Entry {
        name: name.into(),
        step: Some(Box::new(step)),
        status: output(status),
    }
}

fn assert_retirement(fault_panics: bool, drop_panics: bool) {
    let data = ring::<Imu>(2);
    let producer_status = ring::<SystemStatus>(2);
    let failed_status = ring::<SystemStatus>(1);
    let healthy_status = ring::<SystemStatus>(1);
    let calls = Rc::new(Calls::default());
    let seen = Rc::new(RefCell::new(Vec::with_capacity(32)));
    let failed = Runner {
        system: FailingConsumer {
            calls: calls.clone(),
            fault_panics,
            drop_panics,
        },
        state: (),
        inputs: ConsumerInputs {
            data: input(&data),
            status: input(&producer_status),
        },
        outputs: (),
    };
    let healthy = HealthyConsumer {
        inputs: ConsumerInputs {
            data: input(&data),
            status: input(&producer_status),
        },
        seen: seen.clone(),
    };
    let mut status = input::<SystemStatus>(&failed_status);
    let mut coordinator = Coordinator {
        entries: vec![
            entry("producer", Producer(output(&data)), &producer_status),
            entry("failed", failed, &failed_status),
            entry("healthy", healthy, &healthy_status),
        ],
        clock: Clock::Simulated {
            dt: std::time::Duration::from_millis(1),
        },
        epoch: Timestamp(0),
        cycle: 0,
        rings: vec![data, producer_status, failed_status, healthy_status],
    };

    for cycle in 1..=32 {
        coordinator.step(Timestamp(cycle));
        assert_eq!(coordinator.latched().collect::<Vec<_>>(), ["failed"]);
        assert_eq!(calls.execute.get(), 1);
        assert_eq!(calls.fault.get(), 1);
        assert_eq!(calls.drop.get(), 1);
        let status = status.latest().expect("valid status").expect("new status");
        assert_eq!(status.timestamp, Timestamp(cycle));
        if cycle > 1 {
            assert_eq!(status.exec_time_ns, 0);
        }
        assert_eq!(seen.borrow().len(), cycle as usize);
        assert_eq!(
            seen.borrow().last(),
            Some(&(Timestamp(cycle), Timestamp(cycle)))
        );
    }
    drop(coordinator);
    assert_eq!(calls.drop.get(), 1);
}

#[test]
fn failed_consumer_releases_data_and_status_readers() {
    assert_retirement(false, false);
}

#[test]
fn fault_hook_panic_still_retires_the_consumer_once() {
    assert_retirement(true, false);
}

#[test]
fn destructor_panic_still_releases_ports_and_keeps_later_entries_running() {
    assert_retirement(false, true);
}

#[test]
fn fault_and_destructor_panics_are_contained_independently() {
    assert_retirement(true, true);
}
