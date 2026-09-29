use std::cell::{Cell, RefCell};
use std::rc::Rc;

use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;

use super::run::{Coordinator, Entry};
use super::{Clock, EndReason, Step, SystemStatus};
use crate::port::{Input, Output};
use crate::tests::utils::{Imu, input, output, ring};

#[derive(Default)]
struct Calls {
    execute: Cell<usize>,
    fault: Cell<usize>,
    drop: Cell<usize>,
}

struct ConsumerInputs {
    data: Input<Imu>,
    status: Input<SystemStatus>,
}

struct FailingConsumer {
    inputs: ConsumerInputs,
    calls: Rc<Calls>,
    fault_panics: bool,
    drop_panics: bool,
}

impl Step for FailingConsumer {
    fn execute(&mut self, _: Timestamp) {
        self.calls.execute.set(self.calls.execute.get() + 1);
        assert!(self.inputs.data.latest().expect("valid data").is_some());
        assert!(self.inputs.status.latest().expect("valid status").is_some());
        // PANIC Safety: the coordinator catches the failed cycle.
        panic!("consumer failed");
    }

    fn fault(&mut self, _: Timestamp, message: &str) {
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

fn entry(name: &str, step: impl Step + 'static, status: &RingBuffer) -> Entry {
    Entry {
        name: name.into(),
        step: Some(Box::new(step)),
        status: output(status),
    }
}

fn coordinator(entries: Vec<Entry>, rings: Vec<RingBuffer>) -> Coordinator {
    Coordinator {
        entries,
        clock: Clock::Simulated {
            dt: std::time::Duration::from_millis(1),
        },
        epoch: Timestamp(0),
        cycle: 0,
        rings,
    }
}

fn assert_retirement(fault_panics: bool, drop_panics: bool) {
    let data = ring::<Imu>(2, 2);
    let producer_status = ring::<SystemStatus>(2, 2);
    let failed_status = ring::<SystemStatus>(2, 1);
    let healthy_status = ring::<SystemStatus>(2, 1);
    let calls = Rc::new(Calls::default());
    let seen = Rc::new(RefCell::new(Vec::with_capacity(32)));
    let failed = FailingConsumer {
        inputs: ConsumerInputs {
            data: input(&data),
            status: input(&producer_status),
        },
        calls: calls.clone(),
        fault_panics,
        drop_panics,
    };
    let healthy = HealthyConsumer {
        inputs: ConsumerInputs {
            data: input(&data),
            status: input(&producer_status),
        },
        seen: seen.clone(),
    };
    let mut status = input::<SystemStatus>(&failed_status);
    let mut coordinator = coordinator(
        vec![
            entry("producer", Producer(output(&data)), &producer_status),
            entry("failed", failed, &failed_status),
            entry("healthy", healthy, &healthy_status),
        ],
        vec![data, producer_status, failed_status, healthy_status],
    );

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
fn test_failed_consumer_releases_readers() {
    assert_retirement(false, false);
}

#[test]
fn test_fault_panic_retires_consumer() {
    assert_retirement(true, false);
}

#[test]
fn test_destructor_panic_cleanup() {
    assert_retirement(false, true);
}

#[test]
fn test_fault_and_destructor_panic_isolation() {
    assert_retirement(true, true);
}

/// A step that counts its cycles and completes on the second one.
pub(super) struct Twice(pub Rc<Cell<u64>>, pub Option<EndReason>);

impl Step for Twice {
    fn execute(&mut self, _now: Timestamp) {
        self.0.set(self.0.get() + 1);
        if self.0.get() == 2 {
            self.1 = Some(EndReason::Completed);
        }
    }

    fn ended(&self) -> Option<&EndReason> {
        self.1.as_ref()
    }
}

#[test]
fn test_ended_step_stays_and_is_skipped() {
    let status_ring = ring::<SystemStatus>(2, 1);
    let mut status = input::<SystemStatus>(&status_ring);
    let runs = Rc::new(Cell::new(0));
    let mut coordinator = coordinator(
        vec![entry("twice", Twice(runs.clone(), None), &status_ring)],
        vec![status_ring.clone()],
    );
    for cycle in 1..=4 {
        coordinator.step(Timestamp(cycle));
        let seen = status.latest().expect("valid status").expect("new status");
        assert_eq!(seen.timestamp, Timestamp(cycle));
        if cycle > 2 {
            assert_eq!(seen.exec_time_ns, 0);
        }
    }
    assert_eq!(runs.get(), 2);
    assert_eq!(
        coordinator.ended().collect::<Vec<_>>(),
        [("twice", &EndReason::Completed)]
    );
    assert_eq!(coordinator.latched().count(), 0);
}
