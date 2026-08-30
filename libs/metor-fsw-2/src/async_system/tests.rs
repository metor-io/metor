//! The free-running path end to end: a system that awaits one IMU through the
//! ring `Notifier`, publishes a nav, and returns when its context is
//! cancelled. Every port is built by hand on in-memory rings, with no
//! coordinator involved.

use metor_fsw_ring::{Config, NoWake, Notifier, RingBuffer};
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::{AsyncContext, AsyncSystem};
use crate::{
    AsVTable, Frame, FrameList, Input, LogEvent, LogPort, MsgOut, Out, Output, SlotState,
    StatusPort, System, SystemInput, SystemOutput, SystemStatus, buffer_capacity,
};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
    accel: f64,
}

#[derive(AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
struct Residual {
    value: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "nav")]
struct NavEstimate {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    angle: f64,
    residuals: FrameList<Residual, 4>,
}

fn ring_for<F: Frame>(depth: usize, readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: buffer_capacity::<F>(depth),
        max_readers: readers,
    })
}

/// A byte ring for the implicit `LogEvent` message tail.
fn log_ring_for(readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: crate::capacity_for(crate::MAX_MSG_BYTES, 16),
        max_readers: readers,
    })
}

struct AsyncFilter;

#[derive(SystemInput)]
struct AsyncIn {
    imu: Input<Imu, Notifier>,
}

#[derive(SystemOutput)]
struct AsyncOut {
    nav: Output<NavEstimate, Notifier>,
}

impl System for AsyncFilter {
    type Input = AsyncIn;
    type Output = Out<AsyncOut, Notifier>;
    const NAME: &'static str = "async_filter";
}

impl AsyncSystem for AsyncFilter {
    async fn run(
        &mut self,
        context: &mut AsyncContext,
        input: &mut Self::Input,
        output: &mut Self::Output,
    ) {
        loop {
            let Some(Ok(imu)) = context.until_cancelled(input.imu.recv()).await else {
                return;
            };
            let s = imu.get();
            let nav = NavEstimate {
                timestamp: s.timestamp,
                angle: s.omega,
                residuals: FrameList::EMPTY,
            };
            let _ = output.nav.write(&nav);
            // A free-running system closes its own status record.
            context.status().tick(0);
        }
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn async_filter_one_cycle() {
    let imu_ring = ring_for::<Imu>(8, 2);
    let nav_ring = ring_for::<NavEstimate>(8, 2);
    let status_ring = ring_for::<SystemStatus>(8, 1);
    let log_ring = log_ring_for(1);

    let imu_data = Notifier::default();
    let nav_data = Notifier::default();

    let mut input = AsyncIn {
        imu: Input::new(imu_ring.view(imu_data.clone()).unwrap()),
    };
    let mut nav_in = Input::<NavEstimate>::new(nav_ring.view(NoWake).unwrap());
    let mut status_in = Input::<SystemStatus>::new(status_ring.view(NoWake).unwrap());
    let log = LogPort::new(MsgOut::<LogEvent, Notifier>::new(
        log_ring.writer(Notifier::default()).unwrap(),
    ));
    let mut output = Out::new(
        AsyncOut {
            nav: Output::new(nav_ring.writer(nav_data.clone()).unwrap()),
        },
        log,
    );
    let status = StatusPort::new(Output::new(status_ring.writer(NoWake).unwrap()));

    // Feed one IMU sample from a spawned task; the system's `run` awaits it.
    let writer = {
        let imu_ring = imu_ring.clone();
        let imu_data = imu_data.clone();
        stellarator::spawn(async move {
            let mut w = imu_ring.writer(imu_data).unwrap();
            w.try_write(
                Imu {
                    timestamp: Timestamp(7),
                    omega: 2.0,
                    accel: 0.0,
                }
                .as_bytes(),
            )
            .unwrap();
        })
    };

    let cancel = stellarator::util::CancelToken::new();
    let cancel_after_one = cancel.clone();
    let canceller = stellarator::spawn(async move {
        stellarator::yield_now().await;
        cancel_after_one.cancel();
    });
    let mut context = AsyncContext { cancel, status };
    let mut sys = AsyncFilter;
    sys.run(&mut context, &mut input, &mut output).await;
    let _ = canceller.await;
    let _ = writer.await;

    let nav = nav_in
        .latest()
        .expect("ring readable")
        .expect("async system produced a nav");
    assert_eq!(nav.get().angle, 2.0);
    assert_eq!(nav.get().timestamp, Timestamp(7));
    // The loop ticked its own status once per iteration.
    let status = status_in
        .latest()
        .expect("ring readable")
        .expect("the system published its status");
    assert_eq!(status.cycles, 1);
    assert_eq!(status.state, SlotState::Running.code());
}
