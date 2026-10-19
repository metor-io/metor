//! Frames, systems, and configs the unit tests share.

// Most of the tests that use these need the host.
#![cfg_attr(not(feature = "host"), allow(dead_code))]

use core::cell::RefCell;
use core::future::Future;
use core::task::Poll;
use std::rc::Rc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use metor_fsw_3_ring::{Config, NoWake, Notifier, RingBuffer};

use crate::Frame;
use crate::async_system::Stop;
use crate::def::{DefCx, DefError};
use crate::port::{DynInputs, DynOutputs, Input, Output, ring_capacity};
use crate::record::Record;
use crate::system::{System, SystemDef};

use crate::coordinator::{
    BuildError, CoordinatorConfig, InputConfig, SystemConfig, SystemStatus, SystemTable,
};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "imu")]
#[repr(C)]
pub struct Imu {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub sample: f64,
}

/// Another frame named `imu`, sampling two floats where [`Imu`] samples one
/// double: the same bytes under a different layout.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "imu")]
#[repr(C)]
pub struct ImuPair {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub sample: [f32; 2],
}

impl Imu {
    pub fn new(timestamp: i64, sample: f64) -> Self {
        Self {
            timestamp: Timestamp(timestamp),
            sample,
        }
    }
}

/// A frame with no `name`, so its name is the snake-cased ident.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct BareName {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub value: u64,
}

/// A message with a static postcard bound, so it states no length.
#[derive(
    crate::Record, crate::MaxSize, crate::Schema, Serialize, Deserialize, Debug, PartialEq,
)]
pub struct Fixed {
    pub a: u32,
    pub b: f64,
}

/// A message with a string, so it states its length.
#[derive(crate::Record, crate::Schema, Serialize, Deserialize, Debug, PartialEq)]
#[record(max_len = 64, depth = 4)]
pub struct Note {
    pub text: String,
}

/// A message with a timestamp, so fan-in orders it like a frame.
#[derive(crate::Record, crate::Schema, Serialize, Deserialize, Debug, PartialEq)]
#[record(max_len = 64)]
pub struct Stamped {
    #[record(timestamp)]
    pub at: Timestamp,
    pub text: String,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[frame(name = "nav")]
#[repr(C)]
pub struct Nav {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub estimate: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[frame(name = "control")]
#[repr(C)]
pub struct Control {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub command: f64,
}

/// Declares an output under the coordinator's reserved name.
#[crate::system]
pub fn reserved(status: &mut Output<Imu>, now: Timestamp) {
    let _ = status.write(&Imu::new(now.0, 0.0));
}

/// Counts up and publishes the count as a sample.
#[crate::system]
pub fn imu_source(#[state] tick: &mut i64, imu: &mut Output<Imu>) {
    *tick += 1;
    let _ = imu.write(&Imu::new(*tick, *tick as f64));
}

/// A second `imu` producer, always ahead of [`imu_source`] in time.
#[crate::system]
pub fn imu_offset(#[state] tick: &mut i64, imu: &mut Output<Imu>) {
    *tick += 1;
    let _ = imu.write(&Imu::new(*tick + 100, *tick as f64 * 10.0));
}

/// Doubles the newest sample, spinning long enough to be timed.
#[crate::system]
pub fn nav_filter(imu: &mut Input<Imu>, nav: &mut Output<Nav>) {
    spin();
    let Ok(Some(imu)) = imu.latest() else {
        return;
    };
    let _ = nav.write(&Nav {
        timestamp: imu.timestamp,
        estimate: imu.sample * 2.0,
    });
}

/// Publishes a command and records what it published.
#[crate::system]
pub fn control_law(
    #[state] recorder: &mut Recorder,
    nav: &mut Input<Nav>,
    control: &mut Output<Control>,
) {
    let Ok(Some(nav)) = nav.latest() else {
        return;
    };
    let command = Control {
        timestamp: nav.timestamp,
        command: nav.estimate + 1.0,
    };
    recorder
        .0
        .borrow_mut()
        .commands
        .push((command.timestamp, command.command));
    let _ = control.write(&command);
}

/// Reads other systems' status rings like any other input.
#[crate::system]
pub fn status_watch(#[state] recorder: &mut Recorder, status: &mut Input<SystemStatus>) {
    let mut recorded = recorder.0.borrow_mut();
    recorded.statuses.extend(status.drain().flatten().copied());
}

/// Publishes a sample, then panics from its second cycle on.
#[derive(Default)]
pub struct Boom(u64);

/// Fails on its second cycle.
#[crate::system]
fn boom(#[state] s: &mut Boom, imu: &mut Output<Imu>, now: Timestamp) {
    s.0 += 1;
    assert!(s.0 < 2, "boom on cycle {}", s.0);
    let _ = imu.write(&Imu::new(now.0, 1.0));
}

/// Creates a ring for `depth` records of `max_len` bytes, with `readers` slots.
pub fn ring_of(max_len: usize, depth: usize, readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: ring_capacity(max_len, depth).expect("valid capacity"),
        max_readers: readers,
    })
    .expect("a valid ring config")
}

/// Creates a ring for `depth` `T` records, with `readers` slots.
pub fn ring<T: Record + ?Sized>(depth: usize, readers: usize) -> RingBuffer {
    ring_of(T::MAX_LEN, depth, readers)
}

pub fn input<T: Record + ?Sized>(ring: &RingBuffer) -> Input<T> {
    Input::new(vec![ring.view(NoWake).expect("free slot")])
}

pub fn output<T: Record + ?Sized>(ring: &RingBuffer) -> Output<T> {
    Output::new(ring.writer(NoWake).expect("free writer"))
}

/// Creates a one-reader ring for `depth` `T` records, plus its two ports.
pub fn pair<T: Record + ?Sized>(depth: usize) -> (RingBuffer, Output<T>, Input<T>) {
    let ring = ring::<T>(depth, 1);
    let (output, input) = (output(&ring), input(&ring));
    (ring, output, input)
}

/// A trait-path system that panics, so it has no `log` port to fault onto.
pub struct Trap;

impl System for Trap {
    type State = ();
    type Inputs = ();
    type Outputs = ();

    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        SystemDef::new::<(), (), NoWake>("trap", cx)
    }

    fn execute(&self, _now: Timestamp, _state: &mut (), _inputs: &mut (), _outputs: &mut ()) {
        panic!("trap")
    }
}

/// Records the port name and bytes of every dynamic input, each cycle.
pub struct Tap(pub Recorder);

#[crate::system]
pub fn tap(#[state] s: &mut Tap, inputs: &mut DynInputs) {
    for (def, input) in inputs.iter_mut() {
        for record in input.drain() {
            let Ok(bytes) = record else { continue };
            s.0.0
                .borrow_mut()
                .taps
                .push((def.name.to_string(), bytes.to_vec()));
        }
    }
}

/// Writes one fixed record onto every port the config gave it.
pub struct Emit(pub Vec<u8>);

#[crate::system]
pub fn emit(#[state] s: &mut Emit, outputs: &mut DynOutputs) {
    for (_, output) in outputs.iter_mut() {
        let _ = output.write(&s.0);
    }
}

/// Publishes one estimate per sample.
#[crate::system]
async fn relay(imu: &mut Input<Imu, Notifier>, nav: &mut Output<Nav>, stop: Stop) {
    while let Some(sample) = next(imu, &stop).await {
        let _ = nav.write(&Nav {
            timestamp: sample.timestamp,
            estimate: sample.sample * 2.0,
        });
    }
}

/// Reads nothing, so its input mirror fills and drops.
#[crate::system]
async fn sleeper(imu: &mut Input<Imu, Notifier>, stop: Stop) {
    let _ = imu;
    stop.wait().await;
}

/// Panics once it has read one record.
#[crate::system]
async fn async_boom(imu: &mut Input<Imu, Notifier>, stop: Stop) {
    if next(imu, &stop).await.is_some() {
        panic!("boom on the background thread");
    }
}

/// Publishes the hash of its thread's id once, then waits for stop.
#[crate::system]
async fn who_am_i(nav: &mut Output<Nav>, stop: Stop) {
    let _ = nav.write(&Nav {
        timestamp: Timestamp(0),
        estimate: thread_id() as f64,
    });
    stop.wait().await;
}

/// Waits for the next sample, or returns `None` once `stop` is set.
async fn next(imu: &mut Input<Imu, Notifier>, stop: &Stop) -> Option<Imu> {
    futures_lite::future::or(async { imu.next().await.ok().copied() }, async {
        stop.wait().await;
        None
    })
    .await
}

/// Returns this thread's ID, hashed into a number a frame can carry.
pub fn thread_id() -> u64 {
    use core::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    hasher.finish() % (1 << 40)
}

/// Records every log line it is wired to.
pub struct LogSink(Recorder);

#[crate::system]
fn log_sink(#[state] s: &mut LogSink, lines: &mut Input<LogEvent>) {
    s.0.0.borrow_mut().logs.extend(lines.drain().flatten());
}

/// What the recording systems saw, shared with the test that built them.
#[derive(Clone, Default)]
pub struct Recorder(Rc<RefCell<Recorded>>);

#[derive(Default)]
struct Recorded {
    commands: Vec<(Timestamp, f64)>,
    statuses: Vec<SystemStatus>,
    logs: Vec<LogEvent>,
    taps: Vec<(String, Vec<u8>)>,
}

impl Recorder {
    pub fn take(&self) -> Vec<(Timestamp, f64)> {
        core::mem::take(&mut self.0.borrow_mut().commands)
    }

    pub fn take_status(&self) -> Vec<SystemStatus> {
        core::mem::take(&mut self.0.borrow_mut().statuses)
    }

    pub fn take_logs(&self) -> Vec<LogEvent> {
        core::mem::take(&mut self.0.borrow_mut().logs)
    }

    pub fn take_taps(&self) -> Vec<(String, Vec<u8>)> {
        core::mem::take(&mut self.0.borrow_mut().taps)
    }
}

/// A params struct with no fields, so any key is unknown.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct NoParams {}

/// Returns a table of every system type the coordinator tests use.
///
/// The recording systems record into `recorder`.
pub fn table(recorder: &Recorder) -> SystemTable {
    let mut table = SystemTable::new();
    let mut register = || -> Result<(), BuildError> {
        table.register("imu", imu_source(), i64::default)?;
        table.register("imu_offset", imu_offset(), i64::default)?;
        table.register("nav", nav_filter(), |_: NoParams| ())?;
        let control = recorder.clone();
        table.register("control", control_law(), move || control.clone())?;
        let watch = recorder.clone();
        table.register("status_watch", status_watch(), move || watch.clone())?;
        table.register("reserved", reserved(), Default::default)?;
        table.register("boom", boom(), Boom::default)?;
        table.register_system("trap", |_| Ok((Trap, ())))?;
        let logs = recorder.clone();
        table.register("log_sink", log_sink(), move || LogSink(logs.clone()))?;
        let taps = recorder.clone();
        table.register("tap", tap(), move || Tap(taps.clone()))?;
        table.register("emit", emit(), || {
            Emit(Imu::new(1, 5.0).as_bytes().to_vec())
        })?;
        table.register_async("relay", relay(), Default::default)?;
        table.register_async("sleeper", sleeper(), Default::default)?;
        table.register_async("async_boom", async_boom(), Default::default)?;
        table.register_async("ctor_boom", async_boom(), || -> () {
            panic!("a constructor that panics")
        })?;
        table.register_async("who_am_i", who_am_i(), Default::default)?;
        Ok(())
    };
    register().expect("valid records");
    table
}

/// `imu -> nav -> control`, in step order.
pub fn pipeline_config() -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("imu", "imu"),
            SystemConfig {
                inputs: vec![InputConfig::new("imu", "imu", "imu")],
                ..SystemConfig::new("nav", "nav")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("nav", "nav", "nav")],
                ..SystemConfig::new("control", "control")
            },
        ],
        ..Default::default()
    }
}

/// Returns a stop future that resolves after `cycles` polls.
///
/// [`Coordinator::run`] polls it once per cycle.
pub fn after_cycles(cycles: usize) -> impl Future<Output = ()> {
    let mut left = cycles;
    core::future::poll_fn(move |_| {
        left = left.saturating_sub(1);
        if left == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
}

/// Burns a measurable amount of time.
///
/// This gives a status record a nonzero execution time.
fn spin() {
    for i in 0..50_000u64 {
        core::hint::black_box(i);
    }
}

/// The ids of `coordinator`'s systems that ended with `reason`, in step order.
#[cfg(feature = "host")]
pub fn ended_with<'a>(
    coordinator: &'a crate::Coordinator,
    reason: &crate::EndReason,
) -> Vec<&'a str> {
    coordinator
        .systems()
        .filter(|(_, end)| *end == Some(reason))
        .map(|(id, _)| id)
        .collect()
}
