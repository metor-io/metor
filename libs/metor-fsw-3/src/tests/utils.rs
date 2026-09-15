//! Frames, systems, and configs the unit tests share.

use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::port::{Input, Output};
use crate::system::{System, SystemDef};
use crate::{Frame, SystemInputs, SystemOutputs};

use crate::coordinator::{
    CoordinatorConfig, InputConfig, PortRef, SystemConfig, SystemStatus, SystemTable,
};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Debug, PartialEq)]
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

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "nav")]
#[repr(C)]
pub struct Nav {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub estimate: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
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
        drop(imu);
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
        drop(nav);
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

/// What the recording systems saw, shared with the test that built them.
#[derive(Clone, Default)]
pub struct Recorder(Rc<RefCell<Recorded>>);

#[derive(Default)]
struct Recorded {
    commands: Vec<(Timestamp, f64)>,
    statuses: Vec<SystemStatus>,
}

impl Recorder {
    fn push_command(&self, timestamp: Timestamp, command: f64) {
        self.0.borrow_mut().commands.push((timestamp, command));
    }

    fn push_status(&self, status: SystemStatus) {
        self.0.borrow_mut().statuses.push(status);
    }

    pub fn take(&self) -> Vec<(Timestamp, f64)> {
        core::mem::take(&mut self.0.borrow_mut().commands)
    }

    pub fn take_status(&self) -> Vec<SystemStatus> {
        core::mem::take(&mut self.0.borrow_mut().statuses)
    }
}

/// Every system type the coordinator tests name, recording into `recorder`.
pub fn table(recorder: &Recorder) -> SystemTable {
    let mut table = SystemTable::new();
    table.register("imu", || (ImuSource, 0));
    table.register("imu_offset", || (ImuOffset, 0));
    table.register("nav", || (NavFilter, ()));
    let control = recorder.clone();
    table.register("control", move || (ControlLaw, control.clone()));
    let watch = recorder.clone();
    table.register("status_watch", move || (StatusWatch, watch.clone()));
    table.register("reserved", || (Reserved, ()));
    table
}

/// `imu -> nav -> control`, in step order.
pub fn pipeline_config() -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig {
                id: "imu".into(),
                ty: "imu".into(),
                inputs: Vec::new(),
            },
            SystemConfig {
                id: "nav".into(),
                ty: "nav".into(),
                inputs: vec![InputConfig {
                    port: "imu".into(),
                    from: vec![PortRef::new("imu", "imu")],
                }],
            },
            SystemConfig {
                id: "control".into(),
                ty: "control".into(),
                inputs: vec![InputConfig {
                    port: "nav".into(),
                    from: vec![PortRef::new("nav", "nav")],
                }],
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
