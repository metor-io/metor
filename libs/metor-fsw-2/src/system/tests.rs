//! System acceptance tests (system.md "Tests"): the cyclic & async system paths, the
//! self-descriptor + compatibility check, the standard health counters, and the
//! cyclic lapped-input semantics. Ports are built by hand, without a coordinator.

use core::mem::offset_of;
use std::collections::HashMap;

use metor_fsw::Decomponentize;
use metor_fsw_ring::{BoxBacking, Config, NoWake, Notifier, Overrun, RingBuffer};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    AsyncSystem, CyclicRunner, CyclicSystem, Frame, FrameList, HealthPort, Input, Out, Output,
    PortDesc, SystemHealth, SystemInput, SystemKind, SystemLog, SystemOutput, System,
    buffer_capacity, compatible,
};

// ---------------------------------------------------------------------------
// Frames under test: an `Imu` input, a `NavEstimate` output with a dynamic member.
// ---------------------------------------------------------------------------

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
    accel: f64,
}

#[derive(crate::AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
struct Residual {
    value: f64,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "nav")]
struct NavEstimate {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    angle: f64,
    residuals: FrameList<Residual, 4>,
}

/// Records every scalar component it sees as `f64` — for reading a port via its vtable.
#[derive(Default)]
struct RecSink {
    values: HashMap<ComponentId, f64>,
}

impl Decomponentize for RecSink {
    type Error = core::convert::Infallible;
    fn apply_value(
        &mut self,
        id: ComponentId,
        value: ComponentView<'_>,
        _t: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        self.values.insert(id, value.to_f64());
        Ok(())
    }
}

fn overwrite_ring<F: crate::Frame>(depth: usize, readers: usize) -> RingBuffer<BoxBacking> {
    RingBuffer::create_in_memory(Config {
        capacity: buffer_capacity::<F>(depth),
        max_readers: readers,
        overrun: Overrun::Overwrite,
    })
}

// ---------------------------------------------------------------------------
// A sample cyclic system: a unit-gain filter consuming `Imu`, producing `NavEstimate`.
// ---------------------------------------------------------------------------

struct Filter {
    gain: f64,
}

#[derive(SystemInput)]
struct FilterIn {
    imu: Input<Imu>,
}

#[derive(SystemOutput)]
struct FilterOut {
    nav: Output<NavEstimate>,
}

impl System for Filter {
    type Input = FilterIn;
    type Output = Out<FilterOut>;
    const NAME: &'static str = "filter";

    fn init(&mut self, output: &mut Out<FilterOut>) {
        // Publish an initial (default) estimate before the first execute.
        let _ = output.nav.write(&NavEstimate {
            timestamp: Timestamp(0),
            angle: 0.0,
            residuals: FrameList::EMPTY,
        });
    }
}

impl CyclicSystem for Filter {
    // Carries the input's timestamp through (not `now`), so the test can assert the
    // sample stamp survives the cycle.
    fn execute(&mut self, _now: Timestamp, input: &mut FilterIn, output: &mut Out<FilterOut>) {
        // Read the freshest IMU sample; report a health error when starved.
        let (timestamp, angle, accel) = match input.imu.latest() {
            Ok(Some(imu)) => {
                let s = imu.get();
                (s.timestamp, s.omega * self.gain, s.accel)
            }
            _ => {
                output.health().error("imu_missing");
                return;
            }
        };
        // Produce a NavEstimate with a dynamic `residuals` trailer.
        let _ = output.nav.write_with(
            &NavEstimate {
                timestamp,
                angle,
                residuals: FrameList::EMPTY,
            },
            |fw| {
                fw.list(offset_of!(NavEstimate, residuals), |l| {
                    l.push(Residual { value: angle });
                    l.push(Residual { value: accel });
                });
            },
        );
    }
}

#[test]
fn cyclic_filter_end_to_end() {
    let imu_ring = overwrite_ring::<Imu>(8, 2);
    let nav_ring = overwrite_ring::<NavEstimate>(8, 2);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    // Upstream producer + downstream consumer, both built by hand.
    let mut imu_w = Output::<Imu>::new(imu_ring.writer(NoWake, NoWake));
    let mut nav_in = Input::<NavEstimate>::new(nav_ring.view(NoWake, NoWake).unwrap());

    let input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake, NoWake).unwrap()),
    };
    let health = HealthPort::new(
        Output::new(health_ring.writer(NoWake, NoWake)),
        Output::new(log_ring.writer(NoWake, NoWake)),
    );
    let output = Out::new(
        FilterOut {
            nav: Output::new(nav_ring.writer(NoWake, NoWake)),
        },
        health,
    );

    let mut runner = CyclicRunner::new(Filter { gain: 2.0 }, input, output);
    runner.init();

    imu_w
        .write(&Imu {
            timestamp: Timestamp(42),
            omega: 1.5,
            accel: -0.5,
        })
        .unwrap();
    runner.step(Timestamp::now());

    // The consumer reads the produced frame: fixed region zero-copy + dynamic member.
    let nav = nav_in.latest().unwrap().expect("nav produced");
    let est = nav.get();
    assert_eq!(est.angle, 3.0, "omega * gain");
    assert_eq!(est.timestamp, Timestamp(42), "timestamp carried through");
    let residuals = nav.list::<Residual>(offset_of!(NavEstimate, residuals));
    assert_eq!(residuals.len(), 2);
    assert_eq!(residuals.get(0).unwrap().value, 3.0);
    assert_eq!(residuals.get(1).unwrap().value, -0.5);
}

// ---------------------------------------------------------------------------
// is_lapped: a lapped cyclic input is observable (the stop policy lives in the coordinator).
// ---------------------------------------------------------------------------

#[test]
fn cyclic_input_lap_is_observable() {
    let imu_ring = overwrite_ring::<Imu>(2, 1);
    let input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake, NoWake).unwrap()),
    };
    let mut w = imu_ring.writer(NoWake, NoWake);

    assert!(!input.imu.is_lapped());
    assert!(!input.any_lapped());

    // Overrun the small buffer far past capacity without the view advancing.
    for i in 0..32 {
        w.try_write(
            Imu {
                timestamp: Timestamp(i),
                omega: 0.0,
                accel: 0.0,
            }
            .as_bytes(),
        )
        .unwrap();
    }

    assert!(input.imu.is_lapped(), "writer lapped the idle view");
    assert!(input.any_lapped(), "bundle surfaces the lapped port");
}

// ---------------------------------------------------------------------------
// SystemDescriptor + compatibility (subset / ty-shape).
// ---------------------------------------------------------------------------

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuSubset {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64, // a strict subset of Imu's {omega, accel}
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuWrongTy {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f32, // same ids as Imu, different ty (no padding: two f32s fill 8 bytes)
    accel: f32,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuExtra {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
    accel: f64,
    extra: f64, // a field the producer never emits
}

#[test]
fn descriptor_and_compatibility() {
    let desc = <Filter as CyclicSystem>::descriptor();
    assert_eq!(desc.name, "filter");
    assert_eq!(desc.kind, SystemKind::Cyclic);
    assert_eq!(desc.inputs.len(), 1);
    assert_eq!(desc.inputs[0].frame_id(), Imu::FRAME_ID);
    // user nav port + the two implicit health/log ports.
    assert_eq!(desc.outputs.len(), 3);
    assert_eq!(desc.outputs[0].frame_id(), NavEstimate::FRAME_ID);

    let producer = PortDesc::of::<Imu>();
    // A matching subset consumer is compatible.
    assert!(compatible(&producer, &PortDesc::of::<ImuSubset>()));
    // Exact match is compatible.
    assert!(compatible(&producer, &PortDesc::of::<Imu>()));
    // A ty mismatch on a shared component fails.
    assert!(!compatible(&producer, &PortDesc::of::<ImuWrongTy>()));
    // A consumer requiring a field the producer lacks fails.
    assert!(!compatible(&producer, &PortDesc::of::<ImuExtra>()));
    // A different frame id fails.
    assert!(!compatible(&producer, &PortDesc::of::<NavEstimate>()));
}

// ---------------------------------------------------------------------------
// Health: standard counters + a named error counter land on the health port.
// ---------------------------------------------------------------------------

#[test]
fn health_counters_published() {
    let imu_ring = overwrite_ring::<Imu>(8, 1);
    let nav_ring = overwrite_ring::<NavEstimate>(8, 1);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    let mut health_in = Input::<SystemHealth>::new(health_ring.view(NoWake, NoWake).unwrap());

    let input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake, NoWake).unwrap()),
    };
    let health = HealthPort::new(
        Output::new(health_ring.writer(NoWake, NoWake)),
        Output::new(log_ring.writer(NoWake, NoWake)),
    );
    let output = Out::new(
        FilterOut {
            nav: Output::new(nav_ring.writer(NoWake, NoWake)),
        },
        health,
    );

    let mut runner = CyclicRunner::new(Filter { gain: 1.0 }, input, output);
    // No IMU is ever published, so every execute bumps the "imu_missing" error.
    for _ in 0..3 {
        runner.step(Timestamp::now());
    }

    // Read the freshest health record and apply its vtable.
    let record = health_in.latest().unwrap().expect("health published");
    let mut sink = RecSink::default();
    record.apply(&mut sink).unwrap().unwrap();

    assert_eq!(sink.values[&ComponentId::new("health.cycles")], 3.0);
    assert_eq!(sink.values[&ComponentId::new("health.errors")], 3.0);
    assert_eq!(sink.values[&ComponentId::new("health.lapped_inputs")], 0.0);
    assert_eq!(
        sink.values[&ComponentId::new("health.error_counts.imu_missing")],
        3.0,
        "named domain counter lands via the dynamic-frame path"
    );
}

// ---------------------------------------------------------------------------
// A sample async system: awaits one IMU via the ring `Notifier`, produces a nav.
// ---------------------------------------------------------------------------

struct AsyncFilter;

#[derive(SystemInput)]
struct AsyncIn {
    imu: Input<Imu, BoxBacking, Notifier, Notifier>,
}

#[derive(SystemOutput)]
struct AsyncOut {
    nav: Output<NavEstimate, BoxBacking, Notifier, Notifier>,
}

impl System for AsyncFilter {
    type Input = AsyncIn;
    type Output = Out<AsyncOut, BoxBacking, Notifier, Notifier>;
    const NAME: &'static str = "async_filter";
}

impl AsyncSystem for AsyncFilter {
    async fn run(&mut self, input: &mut Self::Input, output: &mut Self::Output) {
        // One end-to-end cycle: await the next IMU, then publish a nav estimate.
        let nav = {
            let Ok(imu) = input.imu.recv().await else {
                return;
            };
            let s = imu.get();
            NavEstimate {
                timestamp: s.timestamp,
                angle: s.omega,
                residuals: FrameList::EMPTY,
            }
        };
        let _ = output.nav.write_async(&nav).await;
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn async_filter_one_cycle() {
    let imu_ring = overwrite_ring::<Imu>(8, 2);
    let nav_ring = overwrite_ring::<NavEstimate>(8, 2);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    let imu_data = Notifier::default();
    let imu_space = Notifier::default();
    let nav_data = Notifier::default();
    let nav_space = Notifier::default();

    let mut input = AsyncIn {
        imu: Input::new(
            imu_ring
                .view(imu_data.clone(), imu_space.clone())
                .unwrap(),
        ),
    };
    let mut nav_in = Input::<NavEstimate>::new(nav_ring.view(NoWake, NoWake).unwrap());
    let health = HealthPort::new(
        Output::new(health_ring.writer(Notifier::default(), Notifier::default())),
        Output::new(log_ring.writer(Notifier::default(), Notifier::default())),
    );
    let mut output = Out::new(
        AsyncOut {
            nav: Output::new(nav_ring.writer(nav_data.clone(), nav_space.clone())),
        },
        health,
    );

    // Feed one IMU sample from a spawned task; the system's `run` awaits it.
    let writer = {
        let imu_ring = imu_ring.clone();
        let imu_data = imu_data.clone();
        let imu_space = imu_space.clone();
        stellarator::spawn(async move {
            let mut w = imu_ring.writer(imu_data, imu_space);
            w.write(
                Imu {
                    timestamp: Timestamp(7),
                    omega: 2.0,
                    accel: 0.0,
                }
                .as_bytes(),
            )
            .await
            .unwrap();
        })
    };

    let mut sys = AsyncFilter;
    sys.run(&mut input, &mut output).await;
    let _ = writer.await;

    let nav = nav_in.latest().unwrap().expect("async system produced a nav");
    assert_eq!(nav.get().angle, 2.0);
    assert_eq!(nav.get().timestamp, Timestamp(7));
}
