//! Frames, systems, and configs the unit tests share.

use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use metor_fsw_3_ring::Notifier;

use crate::async_system::Stop;
use crate::port::{DynInputs, DynOutputs, Input, Output};
use crate::system::{System, SystemDef};
use crate::{Frame, SystemInputs, SystemOutputs};

use crate::coordinator::{
    CoordinatorConfig, InputConfig, PortRef, SystemConfig, SystemStatus, SystemTable,
};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "imu")]
#[repr(C)]
pub struct Imu {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub sample: f64,
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

#[derive(SystemOutputs)]
pub struct ImuOut {
    imu: Output<Imu>,
}

#[derive(SystemInputs)]
pub struct NavIn {
    imu: Input<Imu>,
}

#[derive(SystemOutputs)]
pub struct NavOut {
    nav: Output<Nav>,
}

#[derive(SystemInputs)]
pub struct ControlIn {
    nav: Input<Nav>,
}

#[derive(SystemOutputs)]
pub struct ControlOut {
    control: Output<Control>,
}

#[derive(SystemInputs)]
pub struct StatusIn {
    status: Input<SystemStatus>,
}

#[derive(SystemOutputs)]
pub struct ReservedOut {
    status: Output<Imu>,
}

/// Declares an output under the coordinator's reserved name.
pub struct Reserved;

impl System for Reserved {
    type State = ();
    type Inputs = ();
    type Outputs = ReservedOut;

    fn def() -> SystemDef {
        SystemDef::new::<(), ReservedOut>("reserved")
    }

    fn execute(
        &self,
        now: Timestamp,
        _state: &mut (),
        _inputs: &mut (),
        outputs: &mut ReservedOut,
    ) {
        let _ = outputs.status.write(&Imu {
            timestamp: now,
            sample: 0.0,
        });
    }
}

/// Counts up and publishes the count as a sample.
pub struct ImuSource;

impl System for ImuSource {
    type State = i64;
    type Inputs = ();
    type Outputs = ImuOut;

    fn def() -> SystemDef {
        SystemDef::new::<(), ImuOut>("imu_source")
    }

    fn execute(&self, _now: Timestamp, tick: &mut i64, _inputs: &mut (), outputs: &mut ImuOut) {
        *tick += 1;
        let _ = outputs.imu.write(&Imu {
            timestamp: Timestamp(*tick),
            sample: *tick as f64,
        });
    }
}

/// A second `imu` producer, always ahead of [`ImuSource`] in time.
pub struct ImuOffset;

impl System for ImuOffset {
    type State = i64;
    type Inputs = ();
    type Outputs = ImuOut;

    fn def() -> SystemDef {
        SystemDef::new::<(), ImuOut>("imu_offset")
    }

    fn execute(&self, _now: Timestamp, tick: &mut i64, _inputs: &mut (), outputs: &mut ImuOut) {
        *tick += 1;
        let _ = outputs.imu.write(&Imu {
            timestamp: Timestamp(*tick + 100),
            sample: *tick as f64 * 10.0,
        });
    }
}

/// Doubles the newest sample, spinning long enough to be timed.
pub struct NavFilter;

impl System for NavFilter {
    type State = ();
    type Inputs = NavIn;
    type Outputs = NavOut;

    fn def() -> SystemDef {
        SystemDef::new::<NavIn, NavOut>("nav_filter")
    }

    fn execute(&self, _now: Timestamp, _state: &mut (), inputs: &mut NavIn, outputs: &mut NavOut) {
        spin();
        let Ok(Some(imu)) = inputs.imu.latest() else {
            return;
        };
        let nav = Nav {
            timestamp: imu.timestamp,
            estimate: imu.sample * 2.0,
        };
        let _ = outputs.nav.write(&nav);
    }
}

/// Publishes a command and records what it published.
pub struct ControlLaw;

impl System for ControlLaw {
    type State = Recorder;
    type Inputs = ControlIn;
    type Outputs = ControlOut;

    fn def() -> SystemDef {
        SystemDef::new::<ControlIn, ControlOut>("control_law")
    }

    fn execute(
        &self,
        _now: Timestamp,
        recorder: &mut Recorder,
        inputs: &mut ControlIn,
        outputs: &mut ControlOut,
    ) {
        let Ok(Some(nav)) = inputs.nav.latest() else {
            return;
        };
        let control = Control {
            timestamp: nav.timestamp,
            command: nav.estimate + 1.0,
        };
        recorder.push_command(control.timestamp, control.command);
        let _ = outputs.control.write(&control);
    }
}

/// Reads other systems' status rings like any other input.
pub struct StatusWatch;

impl System for StatusWatch {
    type State = Recorder;
    type Inputs = StatusIn;
    type Outputs = ();

    fn def() -> SystemDef {
        SystemDef::new::<StatusIn, ()>("status_watch")
    }

    fn execute(
        &self,
        _now: Timestamp,
        recorder: &mut Recorder,
        inputs: &mut StatusIn,
        _outputs: &mut (),
    ) {
        for res in inputs.status.drain() {
            let Ok(status) = res else { continue };
            recorder.push_status(*status);
        }
    }
}

/// Publishes a sample, then panics from its second cycle on.
#[derive(Default)]
pub struct Boom(u64);

#[crate::system]
impl Boom {
    /// Fails on its second cycle.
    fn execute(&mut self, imu: &mut Output<Imu>, now: Timestamp) {
        self.0 += 1;
        assert!(self.0 < 2, "boom on cycle {}", self.0);
        let _ = imu.write(&Imu {
            timestamp: now,
            sample: 1.0,
        });
    }
}

/// A trait-path system that panics, so it has no `log` port to fault onto.
pub struct Trap;

impl System for Trap {
    type State = ();
    type Inputs = ();
    type Outputs = ();

    fn def() -> SystemDef {
        SystemDef::new::<(), ()>("trap")
    }

    fn execute(&self, _now: Timestamp, _state: &mut (), _inputs: &mut (), _outputs: &mut ()) {
        panic!("trap")
    }
}

/// Records the port name and bytes of every dynamic input, each cycle.
pub struct Tap(Recorder);

#[crate::system]
impl Tap {
    fn execute(&mut self, inputs: &mut DynInputs) {
        for (def, input) in inputs.iter_mut() {
            for record in input.drain() {
                let Ok(bytes) = record else { continue };
                self.0.push_tap(def.name.to_string(), bytes.to_vec());
            }
        }
    }
}

/// Writes one fixed record onto every port the config gave it.
pub struct Emit(Vec<u8>);

#[crate::system]
impl Emit {
    fn execute(&mut self, outputs: &mut DynOutputs) {
        for (_, output) in outputs.iter_mut() {
            let _ = output.write_bytes(&self.0);
        }
    }
}

/// Doubles every sample as it arrives, on a background thread.
pub struct Relay;

#[crate::system]
impl Relay {
    /// Publishes one estimate per sample.
    async fn run(&mut self, imu: &mut Input<Imu, Notifier>, nav: &mut Output<Nav>, stop: Stop) {
        while let Some(sample) = next(imu, &stop).await {
            let _ = nav.write(&Nav {
                timestamp: sample.timestamp,
                estimate: sample.sample * 2.0,
            });
        }
    }
}

/// Reads nothing, so its input mirror fills and drops.
pub struct Sleeper;

#[crate::system]
impl Sleeper {
    async fn run(&mut self, imu: &mut Input<Imu, Notifier>, stop: Stop) {
        let _ = imu;
        stop.wait().await;
    }
}

/// Panics once it has read one record.
pub struct AsyncBoom;

#[crate::system]
impl AsyncBoom {
    async fn run(&mut self, imu: &mut Input<Imu, Notifier>, stop: Stop) {
        if next(imu, &stop).await.is_some() {
            panic!("boom on the background thread");
        }
    }
}

/// Publishes the hash of its thread's id once, then waits for stop.
pub struct WhoAmI;

#[crate::system]
impl WhoAmI {
    async fn run(&mut self, nav: &mut Output<Nav>, stop: Stop) {
        let _ = nav.write(&Nav {
            timestamp: Timestamp(0),
            estimate: thread_id() as f64,
        });
        stop.wait().await;
    }
}

/// The next sample, or `None` once stop resolves.
async fn next(imu: &mut Input<Imu, Notifier>, stop: &Stop) -> Option<Imu> {
    futures_lite::future::or(async { imu.next().await.ok().copied() }, async {
        stop.wait().await;
        None
    })
    .await
}

/// This thread's id, hashed into a number a frame can carry.
fn thread_id() -> u64 {
    use core::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    hasher.finish() % (1 << 40)
}

/// Records every log line it is wired to.
pub struct LogSink(Recorder);

#[crate::system]
impl LogSink {
    fn execute(&mut self, lines: &mut Input<LogEvent>) {
        for line in lines.drain() {
            let Ok(line) = line else { continue };
            self.0.push_log(line);
        }
    }
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
    fn push_command(&self, timestamp: Timestamp, command: f64) {
        self.0.borrow_mut().commands.push((timestamp, command));
    }

    fn push_status(&self, status: SystemStatus) {
        self.0.borrow_mut().statuses.push(status);
    }

    fn push_log(&self, line: LogEvent) {
        self.0.borrow_mut().logs.push(line);
    }

    fn push_tap(&self, port: String, bytes: Vec<u8>) {
        self.0.borrow_mut().taps.push((port, bytes));
    }

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

/// Every system type the coordinator tests name, recording into `recorder`.
pub fn table(recorder: &Recorder) -> SystemTable {
    let mut table = SystemTable::new();
    table.register_system("imu", |_| Ok((ImuSource, 0)));
    table.register_system("imu_offset", |_| Ok((ImuOffset, 0)));
    table.register_system("nav", |p| p.decode::<NoParams>().map(|_| (NavFilter, ())));
    let control = recorder.clone();
    table.register_system("control", move |_| Ok((ControlLaw, control.clone())));
    let watch = recorder.clone();
    table.register_system("status_watch", move |_| Ok((StatusWatch, watch.clone())));
    table.register_system("reserved", |_| Ok((Reserved, ())));
    table.register("boom", Boom::default);
    table.register_system("trap", |_| Ok((Trap, ())));
    let logs = recorder.clone();
    table.register("log_sink", move || LogSink(logs.clone()));
    let taps = recorder.clone();
    table.register("tap", move || Tap(taps.clone()));
    table.register("emit", || Emit(Imu::new(1, 5.0).as_bytes().to_vec()));
    table.register_async("relay", || Relay);
    table.register_async("sleeper", || Sleeper);
    table.register_async("async_boom", || AsyncBoom);
    table.register_async("ctor_boom", || -> AsyncBoom {
        panic!("a constructor that panics")
    });
    table.register_async("who_am_i", || WhoAmI);
    table
}

/// `imu -> nav -> control`, in step order.
pub fn pipeline_config() -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("imu", "imu"),
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "imu".into(),
                    from: vec![PortRef::new("imu", "imu")],
                }],
                ..SystemConfig::new("nav", "nav")
            },
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "nav".into(),
                    from: vec![PortRef::new("nav", "nav")],
                }],
                ..SystemConfig::new("control", "control")
            },
        ],
        ..Default::default()
    }
}

/// A stop future for [`Coordinator::run`], which polls it once per cycle.
pub fn after_cycles(cycles: usize) -> impl Future<Output = ()> {
    AfterCycles { left: cycles }
}

struct AfterCycles {
    left: usize,
}

impl Future for AfterCycles {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.left = self.left.saturating_sub(1);
        if self.left == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Burns a measurable amount of time so a status record carries a nonzero
/// execution time.
fn spin() {
    for i in 0..50_000u64 {
        core::hint::black_box(i);
    }
}
